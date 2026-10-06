//! 評価後射影形ビュー（本文に `LIMIT`・`ORDER BY`・集計・JOIN を含む `CREATE VIEW`）への
//! 外側の集計・`SELECT DISTINCT`・式 `ORDER BY` の結合テスト（Issue #1411。ポインタ:
//! `docs/spec/04-behavior/table-behavior.md` TABLE-18・`docs/spec/04-behavior/rls.md`
//! RLS-10 (b)・`docs/spec/04-behavior/error-format.md` ERR-1/2/4、TASK-205）。
//!
//! 検証する契約:
//! 1. 外側の形が、本文の結果行を母集合にした Rust 側の独立オラクルと一致する
//!    （本文の `LIMIT` の後の行が集計・並べ替えの対象になる）。
//! 2. 本文は参照したセッション自身の `PolicyContext` で評価され、他テナントの行が
//!    グループ・件数・順序に現れない（3 テナント対照。RLS-10 (b)）。
//! 3. ビューが公開していない物理キー `id` を集計・グループ化・式キーに使えない
//!    （`22000`）。未知列・`VECTOR` 列も `22000`。
//! 4. Describe（本文を実行しない）の結果列は Execute の結果列と一致する。
//!
//! `table18_view.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。

use std::collections::BTreeMap;

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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["alice", "bob", "carol"];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// 3 テナントの基本フィクスチャ。`extra_private` が真なら bob の private 行（高い id・
/// 他の行と異なる言語）を足す（他テナントの private 行の増減が alice・carol の応答を
/// 変えないことの対照用）。
fn seed(storage: &Storage, extra_private: bool) {
    insert_row(storage, "alice", 1, "ja", "a1", Visibility::Public);
    insert_row(storage, "alice", 2, "ja", "a2", Visibility::Public);
    insert_row(storage, "alice", 3, "en", "a3", Visibility::Public);
    insert_row(storage, "alice", 4, "ja", "a4", Visibility::Private);
    insert_row(storage, "bob", 5, "ja", "b5", Visibility::Public);
    insert_row(storage, "bob", 6, "fr", "b6", Visibility::Private);
    insert_row(storage, "carol", 7, "ja", "c7", Visibility::Public);
    if extra_private {
        insert_row(storage, "bob", 8, "zz", "b8", Visibility::Private);
        insert_row(storage, "bob", 9, "zz", "b9", Visibility::Private);
    }
}

fn open_core(name: &str, extra_private: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage, extra_private);
    (new_core(storage), guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn ddl(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = allowed_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn cells(result: &QueryResult) -> Vec<Vec<Cell>> {
    result.rows.iter().map(|r| r.cells.clone()).collect()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn int(v: u64) -> Cell {
    Cell::Integer(v)
}

/// 本文（`top_docs`）の直接実行結果（`(id, lang)`）。外側の独立オラクルの母集合。
fn body_rows(core: &EngineCore, tenant: &str) -> Vec<(u64, String)> {
    let r = run(
        core,
        tenant,
        "SELECT id, lang FROM docs ORDER BY id DESC LIMIT 4",
    )
    .expect("direct body");
    r.rows
        .iter()
        .map(|row| match &row.cells[1] {
            Cell::Text(t) => (row.id, t.clone()),
            other => panic!("unexpected lang cell {other:?}"),
        })
        .collect()
}

fn create_top_docs(core: &EngineCore) {
    ddl(
        core,
        "CREATE VIEW top_docs AS SELECT id, lang, body FROM docs ORDER BY id DESC LIMIT 4",
    )
    .expect("create top_docs");
}

/// 外側の集計・`GROUP BY`・`HAVING`・`DISTINCT`・`ORDER BY`・`LIMIT` が、本文の結果行
/// （本文の `LIMIT` 後）に対する Rust 側のオラクルと一致する（3 テナント対照）。
#[test]
fn outer_aggregates_match_body_result_oracle_per_tenant() {
    let (core, _g) = open_core("buffered-outer-agg", false);
    create_top_docs(&core);
    for tenant in TENANTS {
        let rows = body_rows(&core, tenant);
        let n = rows.len() as u64;
        let mut by_lang: BTreeMap<String, u64> = BTreeMap::new();
        for (_, lang) in &rows {
            *by_lang.entry(lang.clone()).or_default() += 1;
        }

        let r = run(&core, tenant, "SELECT COUNT(*) FROM top_docs").expect("count");
        assert_eq!(cells(&r), vec![vec![int(n)]], "tenant={tenant} count");

        let r = run(
            &core,
            tenant,
            "SELECT lang, COUNT(*) FROM top_docs GROUP BY lang",
        )
        .expect("group by");
        let expected: Vec<Vec<Cell>> = by_lang
            .iter()
            .map(|(l, c)| vec![text(l), int(*c)])
            .collect();
        assert_eq!(cells(&r), expected, "tenant={tenant} group by");

        let r = run(&core, tenant, "SELECT DISTINCT lang FROM top_docs").expect("distinct");
        let expected: Vec<Vec<Cell>> = by_lang.keys().map(|l| vec![text(l)]).collect();
        assert_eq!(cells(&r), expected, "tenant={tenant} distinct");

        let r = run(
            &core,
            tenant,
            "SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang HAVING n >= 2",
        )
        .expect("having");
        let expected: Vec<Vec<Cell>> = by_lang
            .iter()
            .filter(|(_, c)| **c >= 2)
            .map(|(l, c)| vec![text(l), int(*c)])
            .collect();
        assert_eq!(cells(&r), expected, "tenant={tenant} having");

        // ORDER BY は集計項目名・グループキーで指定でき、LIMIT／OFFSET は並べ替えの後。
        let r = run(
            &core,
            tenant,
            "SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang ORDER BY n DESC, lang LIMIT 1",
        )
        .expect("order+limit");
        let mut sorted: Vec<(&String, &u64)> = by_lang.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let top = sorted.first().expect("at least one group");
        assert_eq!(
            cells(&r),
            vec![vec![text(top.0), int(*top.1)]],
            "tenant={tenant} order+limit"
        );

        // 外側 WHERE は本文の LIMIT の後の行を絞り込んでから集計する。
        let ja = by_lang.get("ja").copied().unwrap_or(0);
        let r = run(
            &core,
            tenant,
            "SELECT COUNT(*) FROM top_docs WHERE lang = 'ja'",
        )
        .expect("where+count");
        assert_eq!(cells(&r), vec![vec![int(ja)]], "tenant={tenant} where");

        // 数値集計（公開している id 列は NULL 可能な BIGINT として集計できる）。
        let r = run(
            &core,
            tenant,
            "SELECT MIN(id), MAX(id), COUNT(DISTINCT lang) FROM top_docs",
        )
        .expect("min max");
        let min_id = rows.iter().map(|(i, _)| *i).min().expect("rows");
        let max_id = rows.iter().map(|(i, _)| *i).max().expect("rows");
        assert_eq!(
            cells(&r),
            vec![vec![
                Cell::SignedInteger(min_id as i64),
                Cell::SignedInteger(max_id as i64),
                int(by_lang.len() as u64)
            ]],
            "tenant={tenant} min/max/count distinct"
        );
    }
}

/// 集計は 0 行でも `GROUP BY` なしなら 1 行（`COUNT` は 0・他は NULL）、`GROUP BY` ありなら 0 行。
#[test]
fn outer_aggregate_over_zero_rows() {
    let (core, _g) = open_core("buffered-outer-zero", false);
    create_top_docs(&core);
    let r = run(
        &core,
        "alice",
        "SELECT COUNT(*), MAX(id) FROM top_docs WHERE lang = 'zz'",
    )
    .expect("no group by");
    assert_eq!(cells(&r), vec![vec![int(0), Cell::Null]]);
    let r = run(
        &core,
        "alice",
        "SELECT lang, COUNT(*) FROM top_docs WHERE lang = 'zz' GROUP BY lang",
    )
    .expect("group by");
    assert!(r.rows.is_empty());
    let r = run(
        &core,
        "alice",
        "SELECT DISTINCT lang FROM top_docs WHERE lang = 'zz'",
    )
    .expect("distinct");
    assert!(r.rows.is_empty());
}

/// 集計本文のビューへの外側集計（本文の集計結果列 `n`〔`COUNT`＝BIGINT〕を集計・並べ替える）。
#[test]
fn outer_aggregate_over_aggregate_body_view() {
    let (core, _g) = open_core("buffered-outer-aggbody", false);
    ddl(
        &core,
        "CREATE VIEW lang_counts AS SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
    )
    .expect("create lang_counts");
    for tenant in TENANTS {
        let direct = run(
            &core,
            tenant,
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
        )
        .expect("direct");
        let counts: Vec<u64> = direct
            .rows
            .iter()
            .map(|r| match &r.cells[1] {
                Cell::Integer(v) => *v,
                other => panic!("unexpected count cell {other:?}"),
            })
            .collect();
        let sum: u64 = counts.iter().sum();
        let r = run(
            &core,
            tenant,
            "SELECT SUM(n), MAX(n), MIN(n), COUNT(*) FROM lang_counts",
        )
        .expect("outer sum");
        assert_eq!(
            cells(&r),
            vec![vec![
                Cell::SignedInteger(sum as i64),
                Cell::SignedInteger(*counts.iter().max().expect("groups") as i64),
                Cell::SignedInteger(*counts.iter().min().expect("groups") as i64),
                int(counts.len() as u64)
            ]],
            "tenant={tenant}"
        );
    }
}

/// 外側の式 `ORDER BY`（関数キー・列キーとの混在・降順・`LIMIT`／`OFFSET`）。
#[test]
fn outer_expression_order_by_matches_oracle() {
    let (core, _g) = open_core("buffered-outer-expr-order", false);
    create_top_docs(&core);
    for tenant in TENANTS {
        let rows = body_rows(&core, tenant);
        // lower(lang) 昇順、同値は id 降順。
        let mut expected = rows.clone();
        expected.sort_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
        let r = run(
            &core,
            tenant,
            "SELECT id, lang FROM top_docs ORDER BY lower(lang), id DESC LIMIT 10",
        )
        .expect("expr order");
        let got: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
        let want: Vec<u64> = expected.iter().map(|(i, _)| *i).collect();
        assert_eq!(got, want, "tenant={tenant} lower(lang), id DESC");

        // 降順＋OFFSET／LIMIT は並べ替えの後。
        let mut expected = rows.clone();
        expected.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let r = run(
            &core,
            tenant,
            "SELECT id FROM top_docs ORDER BY lower(lang) DESC, id LIMIT 2 OFFSET 1",
        )
        .expect("expr order desc");
        let got: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
        let want: Vec<u64> = expected.iter().skip(1).take(2).map(|(i, _)| *i).collect();
        assert_eq!(got, want, "tenant={tenant} desc offset limit");
    }
}

/// 他テナントの private 行の増減が、閲覧テナントの外側集計・DISTINCT・式 ORDER BY の応答を
/// 変えない（RLS-10 (b) の対照。bob の private 行を足した core と足さない core の比較）。
#[test]
fn other_tenants_private_rows_do_not_change_outer_results() {
    let (base, _g1) = open_core("buffered-outer-rls-base", false);
    let (extra, _g2) = open_core("buffered-outer-rls-extra", true);
    for core in [&base, &extra] {
        create_top_docs(core);
    }
    let queries = [
        "SELECT COUNT(*) FROM top_docs",
        "SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
        "SELECT DISTINCT lang FROM top_docs",
        "SELECT id FROM top_docs ORDER BY lower(lang), id LIMIT 10",
    ];
    // bob の private 行（8・9）は alice・carol からは見えない。
    for tenant in ["alice", "carol"] {
        for sql in queries {
            let a = run(&base, tenant, sql).expect(sql);
            let b = run(&extra, tenant, sql).expect(sql);
            assert_eq!(
                cells(&a),
                cells(&b),
                "tenant={tenant} sql={sql}: other tenants' private rows leaked"
            );
        }
    }
    // 持ち主の bob には見える（対照: 応答が変わることで、フィクスチャが効いていることを固定する）。
    let a = run(&base, "bob", "SELECT DISTINCT lang FROM top_docs").expect("bob base");
    let b = run(&extra, "bob", "SELECT DISTINCT lang FROM top_docs").expect("bob extra");
    assert_ne!(cells(&a), cells(&b));
}

/// ビューが公開していない物理キー `id` を、集計引数・`GROUP BY`・式キー・`WHERE` のいずれに
/// 使っても `22000`（filter／sort oracle の防止）。未知列・`VECTOR` 列も `22000`。
#[test]
fn outer_forms_reject_unexposed_id_unknown_and_vector_columns() {
    let (core, _g) = open_core("buffered-outer-scope", false);
    ddl(
        &core,
        "CREATE VIEW no_id AS SELECT lang, body FROM docs ORDER BY lang LIMIT 5",
    )
    .expect("create no_id");
    ddl(
        &core,
        "CREATE VIEW with_vec AS SELECT id, embedding FROM docs ORDER BY id LIMIT 5",
    )
    .expect("create with_vec");
    for sql in [
        "SELECT COUNT(id) FROM no_id",
        "SELECT SUM(id) FROM no_id",
        "SELECT id, COUNT(*) FROM no_id GROUP BY id",
        "SELECT DISTINCT id FROM no_id",
        "SELECT COUNT(*) FROM no_id WHERE id = 1",
        "SELECT lang FROM no_id ORDER BY lower(id) LIMIT 5",
        "SELECT lang, COUNT(*) AS n FROM no_id GROUP BY lang HAVING n >= 1 ORDER BY lower(id)",
        "SELECT SUM(nope) FROM no_id",
        "SELECT nope, COUNT(*) FROM no_id GROUP BY nope",
        "SELECT COUNT(embedding) FROM with_vec",
        "SELECT DISTINCT embedding FROM with_vec",
    ] {
        let err = run(&core, "alice", sql).expect_err(sql);
        assert_eq!(err.wire_code(), "22000", "sql={sql}");
    }
}

/// Describe（本文を実行しない）の結果列が Execute の結果列と一致する。
#[test]
fn describe_matches_execute_for_outer_forms() {
    let (core, _g) = open_core("buffered-outer-describe", false);
    create_top_docs(&core);
    for sql in [
        "SELECT COUNT(*) FROM top_docs",
        "SELECT lang, COUNT(*) AS n, MAX(id) AS m FROM top_docs GROUP BY lang",
        "SELECT DISTINCT lang FROM top_docs",
        "SELECT id, lang FROM top_docs ORDER BY lower(lang) LIMIT 3",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "sql={sql}");
    }
}

/// 明示トランザクション内でも同じ経路で評価される。
#[test]
fn outer_aggregate_in_transaction() {
    let (core, _g) = open_core("buffered-outer-txn", false);
    create_top_docs(&core);
    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
        )
        .expect("read in txn");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query");
    };
    let direct = run(
        &core,
        "alice",
        "SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
    )
    .expect("autocommit");
    assert_eq!(cells(&result), cells(&direct));
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "COMMIT")
        .expect("commit");
}
/// 評価後射影形ビューの連鎖（評価後射影形→評価後射影形・単純形→評価後射影形・混在 3 段以上）が
/// 作成でき、参照者ごとの可視行から作った本文を母集合にした独立オラクルと一致する
/// （3 テナント対照。RLS-10 (b)）。
#[test]
fn chained_views_match_oracle_per_tenant() {
    let (core, _g) = open_core("buffered-outer-chain", false);
    create_top_docs(&core);
    for sql in [
        // 評価後射影形 → 評価後射影形（外側の LIMIT は本文の順序の先頭から）。
        "CREATE VIEW v_buf AS SELECT id, lang FROM top_docs LIMIT 3",
        // 単純形 → 評価後射影形（列射影＋宣言的 WHERE）。
        "CREATE VIEW v_simple AS SELECT id, lang FROM top_docs WHERE lang = 'ja'",
        // 集計 → 評価後射影形。
        "CREATE VIEW v_agg AS SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
        // 単純形 → 単純形 → 評価後射影形、評価後射影形 → 単純形の連鎖。
        "CREATE VIEW v_s2 AS SELECT id FROM v_simple",
        "CREATE VIEW v_b3 AS SELECT COUNT(*) AS n FROM v_s2",
    ] {
        ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    for tenant in TENANTS {
        let rows = body_rows(&core, tenant);
        let ja: Vec<u64> = rows
            .iter()
            .filter(|(_, l)| l == "ja")
            .map(|(i, _)| *i)
            .collect();

        let r = run(&core, tenant, "SELECT id FROM v_buf LIMIT 10").expect("v_buf");
        let want: Vec<u64> = rows.iter().take(3).map(|(i, _)| *i).collect();
        let got: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
        assert_eq!(got, want, "tenant={tenant} v_buf");

        let r = run(&core, tenant, "SELECT id FROM v_simple LIMIT 10").expect("v_simple");
        let got: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
        assert_eq!(got, ja, "tenant={tenant} v_simple");

        let r = run(&core, tenant, "SELECT COUNT(*) FROM v_simple").expect("count v_simple");
        assert_eq!(
            cells(&r),
            vec![vec![int(ja.len() as u64)]],
            "tenant={tenant}"
        );

        let r = run(&core, tenant, "SELECT id FROM v_s2 LIMIT 10").expect("v_s2");
        let got: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
        assert_eq!(got, ja, "tenant={tenant} v_s2");

        let r = run(&core, tenant, "SELECT n FROM v_b3 LIMIT 1").expect("v_b3");
        assert_eq!(
            cells(&r),
            vec![vec![int(ja.len() as u64)]],
            "tenant={tenant}"
        );

        let total = rows.len() as i64;
        let r = run(&core, tenant, "SELECT SUM(n) FROM v_agg").expect("v_agg");
        assert_eq!(
            cells(&r),
            vec![vec![Cell::SignedInteger(total)]],
            "tenant={tenant}"
        );
    }
    // 単純形の段が公開しない列は外側から参照できない（`22000`）。
    let err = run(&core, "alice", "SELECT body FROM v_simple LIMIT 1").expect_err("hidden col");
    assert_eq!(err.wire_code(), "22000");
    let err = run(&core, "alice", "SELECT COUNT(body) FROM v_s2").expect_err("hidden col");
    assert_eq!(err.wire_code(), "22000");
    // Describe は Execute と一致する（本文は実行しない）。
    for sql in [
        "SELECT id FROM v_simple LIMIT 10",
        "SELECT n FROM v_b3 LIMIT 1",
        "SELECT lang, SUM(n) AS s FROM v_agg GROUP BY lang",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "sql={sql}");
    }
}

/// 連鎖の深さ上限（テーブルが 0・直接参照するビューが 1。4 を超える作成は `54000`）。評価後射影形
/// 本文が複数の relation を読んでも深さを過小評価しない。CTE・サブクエリ・集合演算の枝・JOIN の
/// 辺からの評価後射影形ビュー参照と、式項目を持つ単純形の段は `42601`（何も永続化されない）。
#[test]
fn chain_depth_limit_and_rejected_positions() {
    let (core, _g) = open_core("buffered-outer-depth", false);
    create_top_docs(&core); // 深さ 1
    ddl(
        &core,
        "CREATE VIEW d2 AS SELECT id, lang FROM top_docs LIMIT 3",
    )
    .expect("d2");
    ddl(&core, "CREATE VIEW d3 AS SELECT id FROM d2").expect("d3");
    ddl(&core, "CREATE VIEW d4 AS SELECT COUNT(*) AS n FROM d3").expect("d4 at the limit");
    let err = ddl(&core, "CREATE VIEW d5 AS SELECT COUNT(*) AS n FROM d4").expect_err("depth");
    assert_eq!(err.wire_code(), "54000");
    let err = ddl(&core, "CREATE VIEW d5 AS SELECT id FROM d4").expect_err("depth");
    assert_eq!(err.wire_code(), "54000");
    for sql in [
        "CREATE VIEW r1 AS WITH x AS (SELECT id FROM top_docs) SELECT id FROM x LIMIT 5",
        "CREATE VIEW r2 AS SELECT id FROM docs WHERE id IN (SELECT id FROM top_docs LIMIT 5) LIMIT 5",
        "CREATE VIEW r3 AS (SELECT id FROM docs) UNION (SELECT id FROM top_docs)",
        "CREATE VIEW r4 AS SELECT docs.id FROM docs INNER JOIN top_docs ON docs.id = top_docs.id LIMIT 5",
        "CREATE VIEW r5 AS SELECT lower(lang) FROM top_docs",
        // 連鎖本文でも組み込み関数以外の呼び出しを含む式述語は拒否される（Issue #1436）。
        "CREATE VIEW r6 AS SELECT id FROM top_docs WHERE no_such_fn(lang) = 'ja' LIMIT 5",
    ] {
        let err = ddl(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
    for name in ["d5", "r1", "r2", "r3", "r4", "r5", "r6"] {
        let err = run(&core, "alice", &format!("SELECT * FROM {name} LIMIT 1"))
            .expect_err("not persisted");
        assert_eq!(err.wire_code(), "42P01", "name={name}");
    }
}

/// 連鎖下でも依存検査は挙動不変: 基底の `DROP TABLE`／`DROP VIEW` は `2BP01`、上位から順に
/// `DROP VIEW` すれば成功し、基底テーブルの `DROP COLUMN` は保守的に拒否される。
#[test]
fn chained_views_keep_dependency_checks() {
    let (core, _g) = open_core("buffered-outer-deps", false);
    create_top_docs(&core);
    ddl(
        &core,
        "CREATE VIEW c2 AS SELECT id, lang FROM top_docs LIMIT 3",
    )
    .expect("c2");
    ddl(&core, "CREATE VIEW c3 AS SELECT id FROM c2").expect("c3");
    for sql in [
        "DROP VIEW top_docs",
        "DROP VIEW c2",
        "DROP TABLE docs",
        "ALTER TABLE docs DROP COLUMN body",
    ] {
        let err = ddl(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "2BP01", "sql={sql}");
    }
    ddl(&core, "CREATE TABLE other (title TEXT, extra TEXT)").expect("other");
    ddl(&core, "ALTER TABLE other DROP COLUMN extra").expect("unrelated table is unaffected");
    for sql in [
        "DROP VIEW c3",
        "DROP VIEW c2",
        "DROP VIEW top_docs",
        "DROP TABLE docs",
    ] {
        ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

/// 連鎖したビューは再オープン後も同じ結果で参照できる（格納本文の再検証の往復）。
#[test]
fn chained_views_persist_across_reopen() {
    let path = unique_db_path("buffered-outer-reopen");
    let _guard = CleanupGuard(path.clone());
    let expected;
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed(&storage, false);
        let core = new_core(storage);
        create_top_docs(&core);
        for sql in [
            "CREATE VIEW p_buf AS SELECT id, lang FROM top_docs LIMIT 3",
            "CREATE VIEW p_simple AS SELECT id FROM p_buf WHERE lang = 'ja'",
            "CREATE VIEW p_count AS SELECT COUNT(*) AS n FROM p_simple",
        ] {
            ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
        expected = cells(&run(&core, "alice", "SELECT n FROM p_count LIMIT 1").expect("read"));
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let core = new_core(storage);
    let again = cells(&run(&core, "alice", "SELECT n FROM p_count LIMIT 1").expect("after reopen"));
    assert_eq!(again, expected);
}
/// 外側のウィンドウ関数（順位関数・`OVER` 付き集計・`PARTITION BY`）が、外側 `WHERE` を通った
/// 本文の結果行を母集合にした独立オラクルと一致する（3 テナント対照）。
#[test]
fn outer_window_functions_match_oracle_per_tenant() {
    let (core, _g) = open_core("buffered-outer-window", false);
    create_top_docs(&core);
    for tenant in TENANTS {
        let rows = body_rows(&core, tenant);

        // ROW_NUMBER() OVER (ORDER BY id): 出力順は本文の順序のまま、値は id 昇順の順位。
        let r = run(
            &core,
            tenant,
            "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM top_docs LIMIT 10",
        )
        .expect("row_number");
        let mut asc: Vec<u64> = rows.iter().map(|(i, _)| *i).collect();
        asc.sort_unstable();
        let expected: Vec<Vec<Cell>> = rows
            .iter()
            .map(|(i, _)| {
                let rn = asc.iter().position(|x| x == i).expect("id") as u64 + 1;
                vec![int(*i), int(rn)]
            })
            .collect();
        assert_eq!(cells(&r), expected, "tenant={tenant} row_number");

        // RANK／DENSE_RANK（同順位は同じ値）と PARTITION BY 付き COUNT。
        let r = run(
            &core,
            tenant,
            "SELECT lang, RANK() OVER (ORDER BY lang) AS rk, DENSE_RANK() OVER (ORDER BY lang) AS dr, COUNT(*) OVER (PARTITION BY lang) AS c FROM top_docs LIMIT 10",
        )
        .expect("rank");
        let mut langs: Vec<&String> = rows.iter().map(|(_, l)| l).collect();
        langs.sort();
        let expected: Vec<Vec<Cell>> = rows
            .iter()
            .map(|(_, l)| {
                let rk = langs.iter().position(|x| *x == l).expect("lang") as u64 + 1;
                let mut distinct: Vec<&String> = langs.clone();
                distinct.dedup();
                let dr = distinct.iter().position(|x| *x == l).expect("lang") as u64 + 1;
                let c = rows.iter().filter(|(_, x)| x == l).count() as u64;
                vec![text(l), int(rk), int(dr), int(c)]
            })
            .collect();
        assert_eq!(cells(&r), expected, "tenant={tenant} rank/dense_rank/count");

        // 母集合は外側 WHERE を通った行（本文の LIMIT の後）。
        let ja = rows.iter().filter(|(_, l)| l == "ja").count() as u64;
        let r = run(
            &core,
            tenant,
            "SELECT id, COUNT(*) OVER () AS c FROM top_docs WHERE lang = 'ja' LIMIT 10",
        )
        .expect("where + window");
        assert_eq!(r.rows.len() as u64, ja, "tenant={tenant}");
        for row in &r.rows {
            assert_eq!(row.cells[1], int(ja), "tenant={tenant}");
        }

        // 文全体の ORDER BY／LIMIT はウィンドウ計算の後（ウィンドウ値は切り出し前の母集合で決まる）。
        let r = run(
            &core,
            tenant,
            "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM top_docs ORDER BY id DESC LIMIT 2",
        )
        .expect("order+limit");
        let n = asc.len() as u64;
        let want: Vec<Vec<Cell>> = asc
            .iter()
            .rev()
            .take(2)
            .map(|i| {
                let rn = asc.iter().position(|x| x == i).expect("id") as u64 + 1;
                vec![int(*i), int(rn)]
            })
            .collect();
        assert_eq!(cells(&r), want, "tenant={tenant} order+limit (n={n})");
    }
    // ウィンドウ別名を WHERE に使う形・非公開列のキー指定の拒否。
    let err = run(
        &core,
        "alice",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM top_docs WHERE rn = 1 LIMIT 5",
    )
    .expect_err("alias in where");
    assert_eq!(err.wire_code(), "42601");
    ddl(
        &core,
        "CREATE VIEW no_id_w AS SELECT lang, body FROM docs ORDER BY lang LIMIT 5",
    )
    .expect("create no_id_w");
    for sql in [
        "SELECT lang, ROW_NUMBER() OVER (ORDER BY id) FROM no_id_w LIMIT 5",
        "SELECT lang, COUNT(*) OVER (PARTITION BY id) FROM no_id_w LIMIT 5",
        "SELECT lang, SUM(id) OVER () FROM no_id_w LIMIT 5",
        "SELECT lang, ROW_NUMBER() OVER (ORDER BY nope) FROM no_id_w LIMIT 5",
    ] {
        let err = run(&core, "alice", sql).expect_err(sql);
        assert_eq!(err.wire_code(), "22000", "sql={sql}");
    }
    // ウィンドウ別名を非公開の物理キー `id` と同名にしても、`WHERE`／`ORDER BY` で `id` を
    // 解決できるようにはならない（絞り込み・並べ替えの oracle にならない）。
    for (sql, code) in [
        (
            "SELECT lang, ROW_NUMBER() OVER () AS id FROM no_id_w WHERE id = 1 LIMIT 5",
            "42601",
        ),
        (
            "SELECT lang, ROW_NUMBER() OVER () AS id FROM no_id_w ORDER BY id LIMIT 5",
            "22000",
        ),
    ] {
        let err = run(&core, "alice", sql).expect_err(sql);
        assert_eq!(err.wire_code(), code, "sql={sql}");
    }
    // Describe は Execute と一致する。
    for sql in [
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM top_docs LIMIT 10",
        "SELECT lang, COUNT(*) OVER (PARTITION BY lang) AS c, id FROM top_docs LIMIT 10",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "sql={sql}");
    }
}

/// 他テナントの private 行の増減が、外側ウィンドウの順位・パーティションを変えない（RLS-10 (b)）。
#[test]
fn other_tenants_private_rows_do_not_change_outer_window_results() {
    let (base, _g1) = open_core("buffered-outer-win-rls-base", false);
    let (extra, _g2) = open_core("buffered-outer-win-rls-extra", true);
    for core in [&base, &extra] {
        create_top_docs(core);
    }
    let sql = "SELECT id, RANK() OVER (PARTITION BY lang ORDER BY id) AS rk, COUNT(*) OVER (PARTITION BY lang) AS c FROM top_docs LIMIT 10";
    for tenant in ["alice", "carol"] {
        let a = run(&base, tenant, sql).expect(sql);
        let b = run(&extra, tenant, sql).expect(sql);
        assert_eq!(cells(&a), cells(&b), "tenant={tenant}: private rows leaked");
    }
}
/// 評価後射影形ビューが複数の relation を読む場合も、深さは本文が読む全 relation の最大深さ + 1
/// で数える（`base_relation` だけを辿ると `u` を深さ 1 と過小評価し、深さ 5 の `w` を受理してしまう）。
#[test]
fn chain_depth_counts_every_relation_of_a_buffered_body() {
    let (core, _g) = open_core("buffered-outer-dag-depth", false);
    for sql in [
        "CREATE VIEW s1 AS SELECT id, lang FROM docs",
        "CREATE VIEW s2 AS SELECT id, lang FROM s1",
        "CREATE VIEW s3 AS SELECT id, lang FROM s2",
        // `base_relation` は docs（深さ 1）だが、s3（深さ 3）も読むため深さ 4。
        "CREATE VIEW u AS (SELECT id FROM docs) UNION (SELECT id FROM s3)",
    ] {
        ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let err = ddl(&core, "CREATE VIEW w AS SELECT COUNT(*) AS n FROM u").expect_err("depth 5");
    assert_eq!(err.wire_code(), "54000");
    let err = run(&core, "alice", "SELECT * FROM w LIMIT 1").expect_err("not persisted");
    assert_eq!(err.wire_code(), "42P01");
}
/// Issue #1436: 連鎖した評価後射影形ビューの本文の `WHERE` に、組み込み関数・算術・数値比較の
/// 式述語を書ける。結果は (a) 同じ SELECT を下位ビューへの外側クエリとして直接実行した結果、
/// (b) 本文の結果行から Rust 側で独立に計算した期待値の双方と一致する（3 テナント対照）。
#[test]
fn chained_body_expression_predicates_match_direct_execution() {
    let (core, _g) = open_core("buffered-outer-expr", false);
    create_top_docs(&core);
    ddl(
        &core,
        "CREATE VIEW v_agg AS SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
    )
    .expect("v_agg");
    ddl(
        &core,
        "CREATE VIEW v_simple AS SELECT id, lang FROM top_docs",
    )
    .expect("v_simple");
    // (作成する名前, 本文の SELECT)
    let cases = [
        ("e_num", "SELECT lang, n FROM v_agg WHERE n > 1 LIMIT 10"),
        (
            "e_fn",
            "SELECT id, lang FROM top_docs WHERE lower(lang) = 'ja' LIMIT 10",
        ),
        (
            "e_arith",
            "SELECT id, lang FROM top_docs WHERE length(lang) + 0 >= 2 LIMIT 10",
        ),
        (
            "e_agg",
            "SELECT lang, COUNT(*) AS c FROM top_docs WHERE upper(lang) = 'JA' GROUP BY lang",
        ),
        (
            "e_simple_layer",
            "SELECT id, lang FROM v_simple WHERE lower(lang) = 'ja' LIMIT 10",
        ),
    ];
    for (name, body) in cases {
        ddl(&core, &format!("CREATE VIEW {name} AS {body}"))
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }
    for tenant in TENANTS {
        let rows = body_rows(&core, tenant);
        for (name, body) in cases {
            let direct = run(&core, tenant, body).unwrap_or_else(|e| panic!("{body}: {e:?}"));
            let via = run(&core, tenant, &format!("SELECT * FROM {name} LIMIT 100"))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(cells(&via), cells(&direct), "tenant={tenant} view={name}");
        }
        // (b) 独立オラクル。
        let ja: Vec<u64> = rows
            .iter()
            .filter(|(_, l)| l == "ja")
            .map(|(i, _)| *i)
            .collect();
        let r = run(&core, tenant, "SELECT id FROM e_fn LIMIT 100").expect("e_fn");
        assert_eq!(
            r.rows.iter().map(|x| x.id).collect::<Vec<_>>(),
            ja,
            "tenant={tenant} e_fn"
        );
        let r = run(&core, tenant, "SELECT id FROM e_simple_layer LIMIT 100").expect("layer");
        assert_eq!(
            r.rows.iter().map(|x| x.id).collect::<Vec<_>>(),
            ja,
            "tenant={tenant} e_simple_layer"
        );
        let mut by_lang: BTreeMap<String, u64> = BTreeMap::new();
        for (_, l) in &rows {
            *by_lang.entry(l.clone()).or_default() += 1;
        }
        let r = run(&core, tenant, "SELECT lang FROM e_num LIMIT 100").expect("e_num");
        let want: Vec<Vec<Cell>> = by_lang
            .iter()
            .filter(|(_, c)| **c > 1)
            .map(|(l, _)| vec![text(l)])
            .collect();
        assert_eq!(cells(&r), want, "tenant={tenant} e_num");
        let r = run(&core, tenant, "SELECT c FROM e_agg LIMIT 100").expect("e_agg");
        let want: Vec<Vec<Cell>> = if ja.is_empty() {
            vec![]
        } else {
            vec![vec![int(ja.len() as u64)]]
        };
        assert_eq!(cells(&r), want, "tenant={tenant} e_agg");
    }
    // Describe は Execute と一致する。
    for sql in [
        "SELECT * FROM e_num LIMIT 100",
        "SELECT id FROM e_fn LIMIT 100",
        "SELECT c FROM e_agg LIMIT 100",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, "alice", sql).expect("execute");
        assert_eq!(described, executed.columns, "sql={sql}");
    }
}

/// Issue #1436 の RLS: 他テナントの private 行（式述語に一致しうる値）の増減が、
/// 連鎖本文の式述語の結果を変えない。
#[test]
fn other_tenants_private_rows_do_not_change_chained_expression_results() {
    let queries = [
        "SELECT id FROM e_zz LIMIT 100",
        "SELECT lang, n FROM e_cnt LIMIT 100",
    ];
    let mut results: Vec<Vec<Vec<Vec<Vec<Cell>>>>> = Vec::new();
    for extra in [false, true] {
        let (core, _g) = open_core(&format!("buffered-outer-expr-rls-{extra}"), extra);
        create_top_docs(&core);
        ddl(
            &core,
            "CREATE VIEW v_cnt AS SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
        )
        .expect("v_cnt");
        ddl(
            &core,
            "CREATE VIEW e_zz AS SELECT id, lang FROM top_docs WHERE lower(lang) = 'zz' LIMIT 10",
        )
        .expect("e_zz");
        ddl(
            &core,
            "CREATE VIEW e_cnt AS SELECT lang, n FROM v_cnt WHERE n > 0 LIMIT 10",
        )
        .expect("e_cnt");
        let mut per = Vec::new();
        for tenant in ["alice", "carol"] {
            per.push(
                queries
                    .iter()
                    .map(|q| cells(&run(&core, tenant, q).expect("query")))
                    .collect::<Vec<_>>(),
            );
        }
        results.push(per);
    }
    assert_eq!(results[0], results[1]);
    // alice・carol には 'zz' の行が見えない。
    assert!(results[1].iter().all(|per| per[0].is_empty()));
}

/// Issue #1436: 式述語つきの連鎖ビューがあっても依存検査（`2BP01`）は不変。
#[test]
fn chained_expression_views_keep_dependency_checks() {
    let (core, _g) = open_core("buffered-outer-expr-deps", false);
    create_top_docs(&core);
    ddl(
        &core,
        "CREATE VIEW x2 AS SELECT id, lang FROM top_docs WHERE lower(lang) = 'ja' LIMIT 3",
    )
    .expect("x2");
    for sql in [
        "DROP VIEW top_docs",
        "DROP TABLE docs",
        "ALTER TABLE docs DROP COLUMN body",
    ] {
        let err = ddl(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "2BP01", "sql={sql}");
    }
    ddl(&core, "DROP VIEW x2").expect("drop upper first");
    ddl(&core, "DROP VIEW top_docs").expect("drop lower");
}

/// Issue #1436: 拒否（fail-closed）。非組み込み関数・セッション UDF・テーブル直下や単純形ビュー
/// 経由の本文の式述語は `42601` で、何も永続化されない。非公開の `id` を式述語で参照する連鎖本文は
/// 参照時に `22000`。
#[test]
fn chained_body_expression_predicates_fail_closed() {
    let (core, _g) = open_core("buffered-outer-expr-reject", false);
    create_top_docs(&core);
    ddl(&core, "CREATE VIEW s_simple AS SELECT id, lang FROM docs").expect("s_simple");
    // セッション UDF を登録しても、本文では呼べない。
    let mut session = allowed_session();
    core.execute_sql_in_session(&ctx("alice"), &mut session, "CREATE FUNCTION f(x) AS x + 1")
        .expect("create function");
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "CREATE VIEW u1 AS SELECT id FROM top_docs WHERE f(id) = 1 LIMIT 5",
        )
        .expect_err("session udf in body");
    assert_eq!(err.wire_code(), "42601");
    for (name, sql) in [
        (
            "u2",
            "CREATE VIEW u2 AS SELECT id FROM top_docs WHERE no_such_fn(lang) = 'ja' LIMIT 5",
        ),
        (
            "u3",
            "CREATE VIEW u3 AS SELECT id FROM docs WHERE lower(lang) = 'ja' LIMIT 5",
        ),
        (
            "u4",
            "CREATE VIEW u4 AS SELECT id FROM s_simple WHERE lower(lang) = 'ja' LIMIT 5",
        ),
        (
            "u5",
            "CREATE VIEW u5 AS SELECT lang, COUNT(*) AS c FROM docs WHERE lower(lang) = 'ja' GROUP BY lang",
        ),
    ] {
        let err = ddl(&core, sql).expect_err(name);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
    for name in ["u1", "u2", "u3", "u4", "u5"] {
        let err = run(&core, "alice", &format!("SELECT * FROM {name} LIMIT 1"))
            .expect_err("not persisted");
        assert_eq!(err.wire_code(), "42P01", "name={name}");
    }
    // id を公開しない下位ビューの上で式述語が id を参照する本文は、参照時に 22000。
    ddl(
        &core,
        "CREATE VIEW no_id AS SELECT lang FROM top_docs LIMIT 3",
    )
    .expect("no_id");
    ddl(
        &core,
        "CREATE VIEW no_id_w AS SELECT lang FROM no_id WHERE id + 0 > 1 LIMIT 5",
    )
    .expect("created; rejected at reference time");
    let err = run(&core, "alice", "SELECT * FROM no_id_w LIMIT 1").expect_err("hidden id");
    assert_eq!(err.wire_code(), "22000");
}

/// Issue #1436: 式述語つきの連鎖ビューは再オープン後も同じ結果で参照できる。
#[test]
fn chained_expression_views_persist_across_reopen() {
    let path = unique_db_path("buffered-outer-expr-reopen");
    let _guard = CleanupGuard(path.clone());
    let expected;
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed(&storage, false);
        let core = new_core(storage);
        create_top_docs(&core);
        for sql in [
            "CREATE VIEW q_agg AS SELECT lang, COUNT(*) AS n FROM top_docs GROUP BY lang",
            "CREATE VIEW q_num AS SELECT lang, n FROM q_agg WHERE n > 1 LIMIT 10",
            "CREATE VIEW q_fn AS SELECT id, lang FROM top_docs WHERE lower(lang) = 'ja' LIMIT 10",
        ] {
            ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
        expected = [
            cells(&run(&core, "alice", "SELECT * FROM q_num LIMIT 100").expect("q_num")),
            cells(&run(&core, "alice", "SELECT * FROM q_fn LIMIT 100").expect("q_fn")),
        ];
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let core = new_core(storage);
    let again = [
        cells(&run(&core, "alice", "SELECT * FROM q_num LIMIT 100").expect("q_num")),
        cells(&run(&core, "alice", "SELECT * FROM q_fn LIMIT 100").expect("q_fn")),
    ];
    assert_eq!(again, expected);
    assert!(!again[0].is_empty() && !again[1].is_empty());
}
