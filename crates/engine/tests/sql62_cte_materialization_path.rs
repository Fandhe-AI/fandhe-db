//! CTE の具体化（一時リレーション化）経路の結合テスト（SQL-62 (1)(3)(5)(6)・Issue #1860、
//! 親 Issue #1859）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・SQL-34・RLS-10 (b)。
//!
//! 明示 `MATERIALIZED` の CTE を主クエリが直接参照すると、本文全体を 1 回評価した結果行集合を
//! 保持して外側がそれを読む。保持行数は `--max-result-rows` で頭打ちになり、超過は結果を
//! 切り詰めず `54000` で拒否される。本テストは (a) 具体化経路で本文が実際に実行されること
//! （外側 `LIMIT` が小さくても本文の可視行が上限を超えれば `54000`）、(b) 上限判定が可視行
//! だけで他テナント行の存在が漏れないこと、(c) 結果がインライン展開と同一であること、
//! (d) 外側が受理できない形のフォールバックを production 経路
//! （`EngineCore::execute_sql_in_session`）で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
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
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件
/// （ja）・private 1 件（ja）。carol: public 1 件（ja）。
/// `table18_view.rs::seed_base_fixture` と同一のフィクスチャ構成
/// （テナント境界検証を同条件で比較できるようにするため）。
fn seed_base_fixture(storage: &Storage) {
    insert_row(
        storage,
        &ctx("alice"),
        1,
        "ja",
        "alice public ja 1",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        2,
        "ja",
        "alice public ja 2",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        3,
        "en",
        "alice public en",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        4,
        "ja",
        "alice private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("bob"),
        5,
        "ja",
        "bob public ja",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("bob"),
        6,
        "ja",
        "bob private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("carol"),
        7,
        "ja",
        "carol public ja",
        Visibility::Public,
    );
}

fn open_with(limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-cte-mat-path");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let core = match limits {
        Some(l) => core.with_result_limits(l),
        None => core,
    };
    (core, guard)
}

fn render(result: &QueryResult) -> String {
    format!("{:?} {:?}", result.columns, result.rows)
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

const MAT: &str = "WITH x AS MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 2";
const PLAIN: &str = "WITH x AS (SELECT * FROM docs) SELECT * FROM x LIMIT 2";
const NOT_MAT: &str = "WITH x AS NOT MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 2";

/// alice の可視行は 6 件（id 1,2,3,4,5,7）。上限 5 なら本文全体の評価が `54000` になり、
/// 外側 `LIMIT 2` では隠れない（本文が実際に実行されている証拠）。インライン版は 2 行で成功する。
#[test]
fn materialized_body_is_fully_evaluated_and_capped() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    assert_eq!(code(&core, "alice", MAT), "54000");
    assert_eq!(query(&core, "alice", PLAIN).expect("plain").rows.len(), 2);
    assert_eq!(
        query(&core, "alice", NOT_MAT).expect("not mat").rows.len(),
        2
    );
}

/// 可視行数がちょうど上限（carol は 5 件）なら具体化版も成功する（境界値）。
#[test]
fn materialized_body_at_cap_boundary_succeeds() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let mat = query(&core, "carol", MAT).expect("at cap");
    let plain = query(&core, "carol", PLAIN).expect("plain");
    assert_eq!(render(&mat), render(&plain));
}

/// 上限判定は RLS 適用後の可視行だけで行う。全テナント合計 7 行は上限 5 を超えるが、
/// carol の可視は 5 行なので成功し、他テナント行の存在は `54000` の有無に表れない。
#[test]
fn cap_counts_only_visible_rows() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let carol = query(&core, "carol", MAT).expect("carol");
    assert!(!result_ids(&carol).contains(&4));
    assert!(!result_ids(&carol).contains(&6));
}

/// 3 テナントで具体化版の ID 集合がインライン版と一致し、他テナントの private 行を含まない。
#[test]
fn materialized_matches_inline_per_tenant() {
    let (core, _g) = open_with(None);
    let mat = "WITH x AS MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 100";
    let plain = "WITH x AS (SELECT * FROM docs) SELECT * FROM x LIMIT 100";
    for tenant in ["alice", "bob", "carol"] {
        let m = query(&core, tenant, mat).expect("mat");
        let p = query(&core, tenant, plain).expect("plain");
        assert_eq!(result_ids(&m), result_ids(&p), "tenant={tenant}");
        assert_eq!(render(&m), render(&p), "tenant={tenant}");
    }
    let alice = query(&core, "alice", mat).expect("alice");
    let carol = query(&core, "carol", mat).expect("carol");
    assert!(!result_ids(&alice).contains(&6));
    assert!(!result_ids(&carol).contains(&4));
    assert!(!result_ids(&carol).contains(&6));
}

/// 外側 WHERE・射影・OFFSET・ORDER BY・外側集計・列名リスト・連鎖でも結果は同一。
#[test]
fn materialized_results_match_inline() {
    let (core, _g) = open_with(None);
    for template in [
        "WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x LIMIT 100",
        "WITH x AS {h}(SELECT id, lang FROM docs WHERE lang = 'ja') \
         SELECT id, lang FROM x WHERE id > 1 LIMIT 100",
        "WITH x AS {h}(SELECT * FROM docs) SELECT lang FROM x ORDER BY lang LIMIT 3 OFFSET 1",
        "WITH x AS {h}(SELECT lang FROM docs) SELECT lang, count(*) FROM x GROUP BY lang",
        "WITH a AS {h}(SELECT lang, body FROM docs), b AS {h}(SELECT lang FROM a) \
         SELECT lang FROM b LIMIT 100",
        "WITH x (l) AS {h}(SELECT lang FROM docs) SELECT l FROM x LIMIT 100",
        "WITH x (l) AS {h}(SELECT lang FROM docs) SELECT * FROM x LIMIT 100",
    ] {
        let plain = template.replace("{h}", "");
        let mat = template.replace("{h}", "MATERIALIZED ");
        let p = query(&core, "alice", &plain).unwrap_or_else(|e| panic!("{plain}: {e:?}"));
        let m = query(&core, "alice", &mat).unwrap_or_else(|e| panic!("{mat}: {e:?}"));
        assert!(!p.rows.is_empty(), "{plain}");
        assert_eq!(render(&m), render(&p), "sql={mat}");
    }
}

/// 外側が評価後射影形として受理できない形（式投影・UDF 述語等）はインラインへフォールバックし、
/// 成功してインライン版と一致する。
#[test]
fn unsupported_outer_falls_back_to_inline() {
    let (core, _g) = open_with(None);
    for template in [
        "WITH x AS {h}(SELECT * FROM docs) SELECT id, lower(lang) FROM x LIMIT 100",
        "WITH x AS {h}(SELECT * FROM docs) SELECT * FROM x WHERE lang ILIKE 'J%' LIMIT 100",
    ] {
        let plain = template.replace("{h}", "");
        let mat = template.replace("{h}", "MATERIALIZED ");
        match query(&core, "alice", &plain) {
            Ok(p) => {
                let m = query(&core, "alice", &mat).expect("fallback must succeed");
                assert_eq!(render(&m), render(&p), "sql={mat}");
            }
            // インラインが拒否する形は具体化指定でも同じコードで拒否される。
            Err(e) => assert_eq!(code(&core, "alice", &mat), e.wire_code(), "sql={mat}"),
        }
    }
}

/// 参照されない `MATERIALIZED` CTE は検証されるが実行されない（上限超過の本文でも `54000` に
/// ならない）。存在しない表は `42P01`。
#[test]
fn unreferenced_materialized_cte_is_validated_but_not_executed() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let ok = "WITH x AS MATERIALIZED (SELECT * FROM docs) SELECT * FROM docs LIMIT 2";
    assert_eq!(
        query(&core, "alice", ok).expect("unreferenced").rows.len(),
        2
    );
    let bad = "WITH x AS MATERIALIZED (SELECT * FROM missing_table) SELECT * FROM docs LIMIT 2";
    assert_eq!(code(&core, "alice", bad), "42P01");
}

/// `ORDER BY` の序数キーは評価後射影形の外側が解決できない（`42601`）ため、具体化を選ばず
/// インライン展開へ戻り、インライン版と同じ結果で成功する（Issue #1860 レビュー指摘）。
#[test]
fn ordinal_order_by_falls_back_to_inline() {
    let (core, _g) = open_with(None);
    let template = "WITH x AS {h}(SELECT lang FROM docs) SELECT lang FROM x ORDER BY 1 LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED ")).expect("mat");
    assert_eq!(plain.rows.len(), 2);
    assert_eq!(render(&mat), render(&plain));
}

/// 非バイト列順の照合の列を持つ表では、具体化結果が照合を運べず実行段が `0A000` で拒否してしまう
/// ため、順序・比較に依存する外側はインライン展開へ戻って従来どおり成功する（Issue #1860
/// レビュー指摘）。
#[test]
fn collated_table_falls_back_to_inline() {
    let path = unique_db_path("sql62-cte-mat-collation");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = new_core(storage);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE ct (id BIGINT PRIMARY KEY, s TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create table");
    for i in 1..=3 {
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut SessionState::default(),
            &format!("INSERT INTO ct (id, s) VALUES ({i}, 'v{i}') USING OPERATION_ID 'ins-{i}'"),
        )
        .expect("insert");
    }
    for template in [
        "WITH x AS {h}(SELECT * FROM ct) SELECT * FROM x WHERE id > 1 LIMIT 2",
        "WITH x AS {h}(SELECT * FROM ct) SELECT * FROM x ORDER BY s LIMIT 2",
    ] {
        let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
        let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "))
            .expect("fallback must succeed");
        assert!(!plain.rows.is_empty());
        assert_eq!(render(&mat), render(&plain), "template={template}");
    }
}

/// 本文が VECTOR 列を結果に含み外側が述語で参照する形は、具体化後に参照不能列（未知列拒否）
/// になるため、インライン展開へ戻って従来どおり成功する（Issue #1860 レビュー指摘）。
#[test]
fn vector_column_predicate_falls_back_to_inline() {
    let (core, _g) = open_with(None);
    let template =
        "WITH x AS {h}(SELECT * FROM docs) SELECT embedding FROM x WHERE lang = 'ja' LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "))
        .expect("fallback must succeed");
    assert_eq!(render(&mat), render(&plain));
}

/// 実カラム `id` を宣言する表の `SELECT *` 本文は、具体化すると外側の `id` 解決が `42702` に
/// なるため、実カラム優先で解決するインライン展開へ戻る（Issue #1860 レビュー指摘）。
#[test]
fn real_id_column_star_body_falls_back_to_inline() {
    let path = unique_db_path("sql62-cte-mat-real-id");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = new_core(storage);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE rt (id BIGINT PRIMARY KEY, s TEXT)",
    )
    .expect("create table");
    for i in 1..=3 {
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut SessionState::default(),
            &format!("INSERT INTO rt (id, s) VALUES ({i}, 'v{i}') USING OPERATION_ID 'rid-{i}'"),
        )
        .expect("insert");
    }
    let template = "WITH x AS {h}(SELECT * FROM rt) SELECT id FROM x LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "))
        .expect("fallback must succeed");
    assert!(!plain.rows.is_empty());
    assert_eq!(render(&mat), render(&plain));
}

/// 先行 CTE が VECTOR 列を改名（列別名）して本文が改名後の名前で読む形も、具体化後は参照不能列に
/// なるため、基底表の物理列へ対応づけて判定しインライン展開へ戻る（Issue #1860 レビュー指摘）。
#[test]
fn renamed_vector_column_through_cte_falls_back_to_inline() {
    let (core, _g) = open_with(None);
    let template =
        "WITH a(e, l) AS (SELECT embedding, lang FROM docs), b AS {h}(SELECT e, l FROM a) \
                    SELECT e FROM b WHERE l = 'ja' LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "))
        .expect("fallback must succeed");
    assert_eq!(render(&mat), render(&plain));
}

/// 本文が同名列を重複投影する形は、具体化すると `42702` になるためインライン展開へ戻り、
/// `MATERIALIZED` の有無で結果が変わらない（Issue #1860 レビュー指摘）。
#[test]
fn duplicate_projection_falls_back_to_inline() {
    let (core, _g) = open_with(None);
    let template = "WITH x AS {h}(SELECT lang, lang FROM docs) SELECT lang FROM x LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", "")).expect("plain");
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "))
        .expect("fallback must succeed");
    assert_eq!(render(&mat), render(&plain));
}

/// 列別名つき CTE の本文が Items 投影になっても、投影しない VECTOR 列のせいで具体化が
/// インラインへ落ちず、`--max-result-rows` の上限が適用される（Issue #1860 レビュー指摘）。
#[test]
fn aliased_cte_still_materializes_and_is_capped() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let mat = "WITH x(l, b) AS MATERIALIZED (SELECT lang, body FROM docs) SELECT l FROM x LIMIT 2";
    assert_eq!(code(&core, "alice", mat), "54000");
    let plain = "WITH x(l, b) AS (SELECT lang, body FROM docs) SELECT l FROM x LIMIT 2";
    assert_eq!(query(&core, "alice", plain).expect("plain").rows.len(), 2);
}

/// 先行 CTE の重複公開列を `SELECT *` で継承する本文は、解決後の投影で重複が現れ、具体化すると
/// `42702` になるためインライン展開へ戻る（Issue #1860 レビュー指摘）。
#[test]
fn inherited_duplicate_columns_fall_back_to_inline() {
    let (core, _g) = open_with(None);
    let template = "WITH a AS (SELECT lang, lang FROM docs), b AS {h}(SELECT * FROM a) \
                    SELECT lang FROM b LIMIT 2";
    let plain = query(&core, "alice", &template.replace("{h}", ""));
    let mat = query(&core, "alice", &template.replace("{h}", "MATERIALIZED "));
    match (plain, mat) {
        (Ok(p), Ok(m)) => assert_eq!(render(&m), render(&p)),
        (Err(p), Err(m)) => assert_eq!(m.wire_code(), p.wire_code()),
        (p, m) => panic!("plain={p:?} mat={m:?}"),
    }
}

/// VECTOR 列を含む本文でも、外側が VECTOR 列を述語等で参照しない素の列射影なら具体化され、
/// `--max-result-rows` の上限が適用される（Issue #1860 レビュー指摘）。
#[test]
fn vector_body_with_plain_outer_still_materializes_and_is_capped() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let mat = "WITH x AS MATERIALIZED (SELECT * FROM docs) SELECT lang FROM x LIMIT 2";
    assert_eq!(code(&core, "alice", mat), "54000");
    let plain = "WITH x AS (SELECT * FROM docs) SELECT lang FROM x LIMIT 2";
    assert_eq!(query(&core, "alice", plain).expect("plain").rows.len(), 2);
}
