//! NULL 許容 VECTOR 列の kNN・hybrid からの NULL 行除外（Issue #2043、親 #2040）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-26 (2)(3)。関連 SQL-33・SQL-74・RLS 相当の境界。
//!
//! `EngineCore::execute_sql_in_session` の production 経路で、NULL 行（0 埋めの区間）が
//! 「マスクが無ければ 1 位になる」配置のとき、NULL 行を除いた表と kNN の id・順序が一致する
//! こと、NULL でない可視行が k 件に満たなければ k 件未満を返すこと、hybrid では NULL 行が疎側の
//! 候補にだけ残ること、他テナントの NULL 行が結果へ影響しないことを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn new_hnsw_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    (
        EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared),
        CleanupGuard(path),
    )
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match exec(core, tenant, sql) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

/// 先頭列（`id` 列）の値を取り出す。疑似列の行 ID と `id` 列の値は別物のため、
/// 結果の同一性はセル値で比べる。
fn ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    query(core, tenant, sql)
        .rows
        .iter()
        .map(|r| first_id(&r.cells))
        .collect()
}

fn first_id(cells: &[Cell]) -> i64 {
    match cells.first() {
        Some(Cell::SignedInteger(v)) => *v,
        Some(Cell::Integer(v)) => i64::try_from(*v).expect("id fits"),
        other => panic!("unexpected id cell {other:?}"),
    }
}

/// 非 NULL 行 `i` のベクトル。原点から離れ、互いに区別できる。
fn vec_of(i: u64) -> String {
    format!("[{},{},{}]", i + 1, (i * 7) % 11 + 1, (i * 3) % 5 + 1)
}

/// 非 NULL 行は偶数 id、`with_nulls` なら奇数 id に NULL 行を交互に挟む。
fn seed(core: &EngineCore, tenant: &str, table: &str, n: u64, with_nulls: bool) {
    for i in 1..=n {
        let sql = format!(
            "INSERT INTO {table} (id, v, body) VALUES ({}, '{}', 'doc {}') \
             USING OPERATION_ID 'o-{table}-{tenant}-{}'",
            i * 2,
            vec_of(i),
            i,
            i * 2
        );
        exec(core, tenant, &sql);
        if with_nulls {
            let sql = format!(
                "INSERT INTO {table} (id, v, body) VALUES ({}, NULL, 'doc null {}') \
                 USING OPERATION_ID 'o-{table}-{tenant}-{}'",
                i * 2 - 1,
                i,
                i * 2 - 1
            );
            exec(core, tenant, &sql);
        }
    }
}

fn create_pair(core: &EngineCore, n: u64) {
    for t in ["t0", "t1"] {
        exec(
            core,
            "sys",
            &format!("CREATE TABLE {t} (id BIGINT PRIMARY KEY, v VECTOR(3), body TEXT)"),
        );
    }
    seed(core, "alice", "t0", n, false);
    seed(core, "alice", "t1", n, true);
}

/// (演算子, クエリ)。`<#>` は負の内積なので、負成分のクエリでは 0 埋めの区間（内積 0）が
/// 「マスクが無ければ 1 位」になる。`<=>` は 0 埋めがゼロノルムの NaN として末尾の枠を消費する。
const OPS: [(&str, &str); 2] = [("<=>", "[0.01,0.01,0.01]"), ("<#>", "[-1,-1,-1]")];
const Q: &str = "[0.01,0.01,0.01]";

/// TABLE-26 (2): NULL を含まない表 T0 と NULL 行を交互に挟んだ T1 で、kNN の id・順序が一致する。
/// 2 回目はアリーナキャッシュのヒット経路を通る。
#[test]
fn knn_matches_table_without_null_rows_cold_and_cached() {
    let (core, _g) = new_core("t26s-eq");
    create_pair(&core, 40);
    for (op, q) in OPS {
        for limit in [1usize, 5, 40] {
            let sql = |t: &str| format!("SELECT id FROM {t} ORDER BY v {op} '{q}' LIMIT {limit}");
            let want = ids(&core, "alice", &sql("t0"));
            assert_eq!(want.len(), limit.min(40), "{op} {limit}");
            let cold = ids(&core, "alice", &sql("t1"));
            let warm = ids(&core, "alice", &sql("t1"));
            assert_eq!(cold, want, "{op} {limit} cold");
            assert_eq!(warm, want, "{op} {limit} cached");
        }
    }
}

/// TABLE-26 (2): 非 NULL の可視行が k に満たなければ k 件未満を返す。全行 NULL なら 0 件（エラーにしない）。
#[test]
fn fewer_non_null_rows_than_k_and_all_null_table() {
    let (core, _g) = new_core("t26s-few");
    create_pair(&core, 3);
    for (op, q) in OPS {
        let r = ids(
            &core,
            "alice",
            &format!("SELECT id FROM t1 ORDER BY v {op} '{q}' LIMIT 100"),
        );
        assert_eq!(r.len(), 3, "{op}");
        assert!(r.iter().all(|id| id % 2 == 0), "{op}: NULL row returned");
    }
    exec(
        &core,
        "sys",
        "CREATE TABLE t_null (id BIGINT PRIMARY KEY, v VECTOR(3))",
    );
    for i in 1..=4u64 {
        exec(
            &core,
            "alice",
            &format!("INSERT INTO t_null (id, v) VALUES ({i}, NULL) USING OPERATION_ID 'n-{i}'"),
        );
    }
    for (op, q) in OPS {
        for _ in 0..2 {
            let r = ids(
                &core,
                "alice",
                &format!("SELECT id FROM t_null ORDER BY v {op} '{q}' LIMIT 10"),
            );
            assert!(r.is_empty(), "{op}");
        }
    }
}

/// `WHERE` 付き（SCALAR 段）・LIMIT 省略・DESC でも NULL 行は出ず、T0 と一致する。
#[test]
fn where_clause_desc_and_unbounded_limit_exclude_null_rows() {
    let (core, _g) = new_core("t26s-where");
    create_pair(&core, 30);
    for sql in [
        "SELECT id FROM {t} WHERE id > 20 ORDER BY v <#> '[-1,-1,-1]' LIMIT 8",
        "SELECT id FROM {t} WHERE id > 20 ORDER BY v <=> '[0.01,0.01,0.01]' LIMIT 8",
        "SELECT id FROM {t} ORDER BY v <=> '[0.01,0.01,0.01]' DESC LIMIT 8",
        "SELECT id FROM {t} ORDER BY v <=> '[0.01,0.01,0.01]'",
        "SELECT id FROM {t} ORDER BY v <#> '[0.01,0.01,0.01]' LIMIT 100",
    ] {
        let want = ids(&core, "alice", &sql.replace("{t}", "t0"));
        let got = ids(&core, "alice", &sql.replace("{t}", "t1"));
        assert_eq!(got, want, "{sql}");
        assert!(!got.is_empty(), "{sql}");
    }
}

/// HNSW opt-in エンジンでも NULL 行は索引にも結果にも入らず、既定エンジンの結果と一致する。
#[test]
fn hnsw_engine_excludes_null_rows_and_matches_exact() {
    let (hnsw, _g1) = new_hnsw_core("t26s-hnsw");
    let (exact, _g2) = new_core("t26s-hnsw-exact");
    for core in [&hnsw, &exact] {
        create_pair(core, 40);
    }
    exec(
        &hnsw,
        "sys",
        "CREATE INDEX i_t1 ON t1 USING hnsw (v) WITH (m = 8, ef_construction = 40)",
    );
    for (op, q) in OPS {
        for _ in 0..2 {
            let sql = format!("SELECT id FROM t1 ORDER BY v {op} '{q}' LIMIT 10");
            let got = ids(&hnsw, "alice", &sql);
            let want = ids(&exact, "alice", &sql);
            assert_eq!(got, want, "{op}");
            assert!(got.iter().all(|id| id % 2 == 0), "{op}: NULL row returned");
        }
    }
}

/// UPDATE で NULL へ変えた行は結果から消え、NULL から値へ変えた行は結果に現れる。
#[test]
fn update_to_and_from_null_is_reflected_in_knn() {
    let (core, _g) = new_core("t26s-upd");
    create_pair(&core, 10);
    let sql = "SELECT id FROM t1 ORDER BY v <#> '[-1,-1,-1]' LIMIT 3".to_string();
    let before = ids(&core, "alice", &sql);
    assert_eq!(before.len(), 3);
    let first = before[0];
    exec(
        &core,
        "alice",
        &format!("UPDATE t1 SET v = NULL WHERE id = {first} USING OPERATION_ID 'u-null'"),
    );
    let after = ids(&core, "alice", &sql);
    assert!(!after.contains(&first));
    assert_eq!(after.len(), 3);
    // NULL だった id=1 の行にクエリと同向きの大きな値を入れると（負の内積が最小になり）1 位に現れる。
    exec(
        &core,
        "alice",
        "UPDATE t1 SET v = '[-1000,-1000,-1000]' WHERE id = 1 USING OPERATION_ID 'u-val'",
    );
    let again = ids(&core, "alice", &sql);
    assert_eq!(again.first(), Some(&1));
}

/// TABLE-26 (3): hybrid では NULL 行が密側の候補から外れ、疎側の候補には残る。
/// 投影した VECTOR 列は NULL。
#[test]
fn hybrid_keeps_null_rows_on_sparse_side_only() {
    let (core, _g) = new_core("t26s-hyb");
    exec(
        &core,
        "sys",
        "CREATE TABLE h (id BIGINT PRIMARY KEY, v VECTOR(3), body TEXT)",
    );
    for i in 1..=6u64 {
        exec(
            &core,
            "alice",
            &format!(
                "INSERT INTO h (id, v, body) VALUES ({i}, '{}', 'plain text {i}') \
                 USING OPERATION_ID 'h-{i}'",
                vec_of(i)
            ),
        );
    }
    exec(
        &core,
        "alice",
        "INSERT INTO h (id, v, body) VALUES (100, NULL, 'zebra zebra zebra') \
         USING OPERATION_ID 'h-100'",
    );
    exec(
        &core,
        "alice",
        "INSERT INTO h (id, v, body) VALUES (101, NULL, 'other words') USING OPERATION_ID 'h-101'",
    );
    for _ in 0..2 {
        let r = query(
            &core,
            "alice",
            "SELECT id, v FROM h ORDER BY HYBRID(v, '[0.01,0.01,0.01]', body, 'zebra') LIMIT 10",
        );
        let got: Vec<i64> = r.rows.iter().map(|x| first_id(&x.cells)).collect();
        // 疎側に一致する NULL 行は結果に現れ、一致しない NULL 行は密側にも疎側にも現れない。
        assert!(got.contains(&100), "{got:?}");
        assert!(!got.contains(&101), "{got:?}");
        let row = r
            .rows
            .iter()
            .find(|x| first_id(&x.cells) == 100)
            .expect("row 100");
        assert!(
            matches!(row.cells.get(1), Some(Cell::Null)),
            "{:?}",
            row.cells
        );
        for x in r.rows.iter().filter(|x| first_id(&x.cells) != 100) {
            assert!(matches!(x.cells.get(1), Some(Cell::Vector(_))));
        }
    }
}

/// 他テナントの NULL 行（および通常行）は自テナントの件数・順位・エラーに影響しない。
#[test]
fn other_tenants_null_rows_do_not_affect_results() {
    let (core, _g) = new_core("t26s-rls");
    create_pair(&core, 12);
    let sql1 = format!("SELECT id FROM t1 ORDER BY v <=> '{Q}' LIMIT 20");
    // alice にだけ NULL 行がある表を bob が検索しても、空で成功する（件数・エラーに差が出ない）。
    assert!(ids(&core, "bob", &sql1).is_empty());
    exec(
        &core,
        "bob",
        &format!(
            "INSERT INTO t1 (id, v, body) VALUES (2, '{}', 'x') USING OPERATION_ID 'bob-1'",
            vec_of(1)
        ),
    );
    assert_eq!(ids(&core, "bob", &sql1), vec![2]);
    assert_eq!(ids(&core, "alice", &sql1).len(), 12);
}
