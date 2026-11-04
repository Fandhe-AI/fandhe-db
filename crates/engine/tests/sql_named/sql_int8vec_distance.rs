//! `INT8VEC(N)` 列への距離演算子 `<->`・`<#>`・`<=>` の結合テスト（TABLE-49 (1)(4)・Issue #2953）。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) kNN（`ORDER BY v <op> '[..]'`）が
//! テスト内で i64 により厳密に求めた参照と一致すること、(2) `DESC`・`OFFSET`・`LIMIT` 省略・NULL と
//! ゼロノルム（NaN）の並び・境界値（−128・127）、(3) WHERE の距離比較（半径検索）・投影の距離列、
//! (4) 未対応の形（`<+>`・hybrid・`USING PLAN`・precision・`HINT ORDER`・関数形・混在）が fail-closed に
//! 拒否されること、(5) テナント境界と行ポリシーが kNN でも保たれること、(6) 同じ値を持つ `vector` 表との
//! 比較と、量子化した f32 データに対する誤差の実測（TABLE-49 (5)。値は docs に転記する）を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for(tenant), &mut s, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) {
    exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn ok(core: &EngineCore, sql: &str) {
    ok_as(core, "alice", sql);
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec_as(core, "alice", sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn query_as(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}")) {
        SqlOutcome::Query(q) => q,
        other => panic!("{sql} must return a query result, got {other:?}"),
    }
}

fn cell_id(cell: Option<&Cell>) -> i64 {
    match cell {
        Some(Cell::SignedInteger(n)) => *n,
        Some(Cell::Integer(n)) => i64::try_from(*n).expect("id"),
        other => panic!("unexpected id cell {other:?}"),
    }
}

/// 先頭列を id として並び順どおりに返す。
fn ids_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    query_as(core, tenant, sql)
        .rows
        .iter()
        .map(|r| cell_id(r.cells.first()))
        .collect()
}

fn ids(core: &EngineCore, sql: &str) -> Vec<i64> {
    ids_as(core, "alice", sql)
}

/// `SELECT id, <距離> AS d ...` の結果を `(id, d)` で返す（NULL は `None`）。
fn id_dists(core: &EngineCore, sql: &str) -> Vec<(i64, Option<f64>)> {
    query_as(core, "alice", sql)
        .rows
        .iter()
        .map(|r| {
            let d = match r.cells.get(1) {
                Some(Cell::Float(f)) => Some(*f),
                Some(Cell::Null) => None,
                other => panic!("unexpected distance cell {other:?}"),
            };
            (cell_id(r.cells.first()), d)
        })
        .collect()
}

/// 決定的な xorshift64*（外部依存なし）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [-128, 127] の一様な i8。
    fn i8(&mut self) -> i8 {
        (self.next() >> 56) as u8 as i8
    }
}

fn lit(v: &[i8]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn dot(a: &[i8], b: &[i8]) -> i64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| i64::from(*x) * i64::from(*y))
        .sum()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    L2,
    Ip,
    Cos,
}

impl Op {
    const ALL: [Op; 3] = [Op::L2, Op::Ip, Op::Cos];

    fn sql(self) -> &'static str {
        match self {
            Op::L2 => "<->",
            Op::Ip => "<#>",
            Op::Cos => "<=>",
        }
    }
}

/// テスト内の参照距離（i64 で厳密に求め、最後にだけ f64 へ落とす。カーネルと同じ式）。
fn ref_dist(op: Op, a: &[i8], q: &[i8]) -> f64 {
    let d = dot(a, q);
    let na = dot(a, a);
    let nq = dot(q, q);
    match op {
        Op::L2 => ((na + nq - 2 * d) as f64).sqrt(),
        Op::Ip => -(d as f64),
        Op::Cos => {
            if na == 0 || nq == 0 {
                f64::NAN
            } else {
                1.0 - (d as f64 / ((na as f64) * (nq as f64)).sqrt()).clamp(-1.0, 1.0)
            }
        }
    }
}

/// 参照の Top-k（距離昇順・同点は id 昇順。NaN は末尾）。
fn ref_top(op: Op, rows: &[(i64, Vec<i8>)], q: &[i8], k: usize) -> Vec<(i64, f64)> {
    let mut all: Vec<(i64, f64)> = rows
        .iter()
        .map(|(id, v)| (*id, ref_dist(op, v, q)))
        .collect();
    all.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    // total_cmp は NaN を正の端に置くので、昇順では末尾になる。
    all.truncate(k);
    all
}

fn make_rows(seed: u64, n: i64, dim: usize) -> Vec<(i64, Vec<i8>)> {
    let mut rng = Rng(seed);
    (1..=n)
        .map(|id| (id, (0..dim).map(|_| rng.i8()).collect()))
        .collect()
}

fn insert_rows(core: &EngineCore, table: &str, rows: &[(i64, Vec<i8>)]) {
    for (id, v) in rows {
        ok(
            core,
            &format!("INSERT INTO {table} (id, v) VALUES ({id}, '{}')", lit(v)),
        );
    }
}

fn setup(
    label: &str,
    dim: usize,
    n: i64,
    seed: u64,
) -> (CleanupGuard, EngineCore, Vec<(i64, Vec<i8>)>) {
    let (g, core) = open(label);
    ok(
        &core,
        &format!("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC({dim}))"),
    );
    let rows = make_rows(seed, n, dim);
    insert_rows(&core, "t", &rows);
    (g, core, rows)
}

/// TABLE-49 (1)(4): 3 演算子の昇順 kNN が厳密な参照（同点は id 昇順）と一致し、距離列も一致する。
#[test]
fn knn_matches_exact_reference_for_all_operators() {
    let (_g, core, rows) = setup("ivd-ref", 8, 60, 0x1234_5678_9abc_def1);
    let q: Vec<i8> = vec![5, -7, 100, -128, 127, 0, 33, -1];
    for op in Op::ALL {
        let sql = format!(
            "SELECT id, v {o} '{}' AS d FROM t ORDER BY v {o} '{}' LIMIT 10",
            lit(&q),
            lit(&q),
            o = op.sql()
        );
        let got = id_dists(&core, &sql);
        let want = ref_top(op, &rows, &q, 10);
        assert_eq!(got.len(), want.len(), "{op:?}");
        for ((gid, gd), (wid, wd)) in got.iter().zip(&want) {
            assert_eq!(gid, wid, "{op:?} id order");
            assert_eq!(gd.expect("distance"), *wd, "{op:?} distance of {gid}");
        }
        // 距離列なしの形も同じ順序。
        let plain = format!(
            "SELECT id FROM t ORDER BY v {} '{}' LIMIT 10",
            op.sql(),
            lit(&q)
        );
        assert_eq!(
            ids(&core, &plain),
            want.iter().map(|w| w.0).collect::<Vec<_>>(),
            "{op:?}"
        );
    }
}

/// `DESC`・`OFFSET`・`LIMIT` 省略。
#[test]
fn desc_offset_and_omitted_limit() {
    let (_g, core, rows) = setup("ivd-desc", 4, 40, 0xdead_beef_1234_5678);
    let q: Vec<i8> = vec![10, -20, 30, -40];
    for op in Op::ALL {
        let asc_all: Vec<i64> = ref_top(op, &rows, &q, rows.len())
            .iter()
            .map(|w| w.0)
            .collect();
        // OFFSET は昇順の並びの途中から。
        let sql = format!(
            "SELECT id FROM t ORDER BY v {} '{}' LIMIT 5 OFFSET 7",
            op.sql(),
            lit(&q)
        );
        assert_eq!(ids(&core, &sql), asc_all[7..12].to_vec(), "{op:?} offset");
        // LIMIT 省略は全行（暗黙上限内）。
        let sql = format!("SELECT id FROM t ORDER BY v {} '{}'", op.sql(), lit(&q));
        assert_eq!(ids(&core, &sql), asc_all, "{op:?} implicit limit");
        // DESC は遠い順（距離の列が非増加）。
        let sql = format!(
            "SELECT id, v {o} '{}' AS d FROM t ORDER BY v {o} '{}' DESC LIMIT 40",
            lit(&q),
            lit(&q),
            o = op.sql()
        );
        let got = id_dists(&core, &sql);
        assert_eq!(got.len(), 40);
        let dists: Vec<f64> = got.iter().map(|g| g.1.expect("d")).collect();
        for w in dists.windows(2) {
            assert!(w[0] >= w[1], "{op:?} must be non-increasing: {dists:?}");
        }
        let mut got_ids: Vec<i64> = got.iter().map(|g| g.0).collect();
        got_ids.sort_unstable();
        assert_eq!(
            got_ids,
            (1..=40).collect::<Vec<_>>(),
            "{op:?} covers all rows"
        );
    }
}

/// NULL 行は昇順の末尾（PG 準拠）、ゼロノルム行のコサイン距離は NaN で昇順の末尾側・降順の先頭側。
#[test]
fn null_rows_and_zero_norm_cosine_follow_pg_order() {
    let (_g, core) = open("ivd-null");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,0]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (2, '[0,1]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (3, '[0,0]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (4, NULL)");
    ok(&core, "INSERT INTO t (id, v) VALUES (5, '[-1,0]')");
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY v <=> '[1,0]' LIMIT 10"),
        vec![1, 2, 5, 3, 4]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY v <=> '[1,0]' DESC LIMIT 10"
        ),
        vec![4, 3, 5, 2, 1]
    );
    let d = id_dists(
        &core,
        "SELECT id, v <=> '[1,0]' AS d FROM t ORDER BY v <=> '[1,0]' LIMIT 10",
    );
    assert!(d[3].1.expect("nan").is_nan(), "zero norm row is NaN");
    assert_eq!(d[4], (4, None), "NULL row distance is NULL");
    // L2 と内積は NULL だけが末尾で、ゼロ行も有限。
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY v <-> '[1,0]' LIMIT 10"),
        vec![1, 3, 2, 5, 4]
    );
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY v <#> '[1,0]' LIMIT 10"),
        vec![1, 2, 3, 5, 4]
    );
}

/// 境界値（−128・127）と dim 1。内積は i32 を溢れさせる大きな dim でも厳密。
#[test]
fn boundary_values_are_exact() {
    let (_g, core) = open("ivd-bound");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(1))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[-128]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (2, '[127]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (3, '[0]')");
    let d = id_dists(
        &core,
        "SELECT id, v <-> '[127]' AS d FROM t ORDER BY v <-> '[127]' LIMIT 3",
    );
    assert_eq!(d, vec![(2, Some(0.0)), (3, Some(127.0)), (1, Some(255.0))]);
    let d = id_dists(
        &core,
        "SELECT id, v <#> '[-128]' AS d FROM t ORDER BY v <#> '[-128]' LIMIT 3",
    );
    // −(127*−128) = 16256、−(0)=0、−(−128*−128) = −16384。
    assert_eq!(
        d,
        vec![(1, Some(-16384.0)), (3, Some(0.0)), (2, Some(16256.0))]
    );

    // 大きな dim: 全要素 −128 同士の内積 = 128*128*dim。
    let dim = 4096usize;
    ok(
        &core,
        &format!("CREATE TABLE big (id BIGINT PRIMARY KEY, v INT8VEC({dim}))"),
    );
    let ext = vec![-128i8; dim];
    ok(
        &core,
        &format!("INSERT INTO big (id, v) VALUES (1, '{}')", lit(&ext)),
    );
    let d = id_dists(
        &core,
        &format!(
            "SELECT id, v <#> '{q}' AS d FROM big ORDER BY v <#> '{q}' LIMIT 1",
            q = lit(&ext)
        ),
    );
    assert_eq!(d, vec![(1, Some(-(128.0 * 128.0 * dim as f64)))]);
}

/// WHERE のスカラー条件との併用・距離比較（半径検索）・投影の距離列。
#[test]
fn where_filter_radius_and_projection() {
    let (_g, core) = open("ivd-where");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), n INTEGER)",
    );
    for (id, v, n) in [
        (1, "[0,0]", 1),
        (2, "[3,4]", 2),
        (3, "[6,8]", 1),
        (4, "[30,40]", 2),
    ] {
        ok(
            &core,
            &format!("INSERT INTO t (id, v, n) VALUES ({id}, '{v}', {n})"),
        );
    }
    // 併用: n = 1 の行だけが候補。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE n = 1 ORDER BY v <-> '[0,0]' LIMIT 10"
        ),
        vec![1, 3]
    );
    // 半径検索: 距離 ≤ 10（WHERE の距離比較）。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE v <-> '[0,0]' <= 10 ORDER BY v <-> '[0,0]' LIMIT 10"
        ),
        vec![1, 2, 3]
    );
    // 左右反転のリテラル（距離は対称）。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE '[0,0]' <-> v < 10 ORDER BY id LIMIT 10"
        ),
        vec![1, 2]
    );
    // 投影の距離列は ORDER BY と同じクエリ・演算子のときだけ受理する（SQL-33 (7)）。
    let d = id_dists(
        &core,
        "SELECT id, v <-> '[0,0]' AS d FROM t ORDER BY v <-> '[0,0]' LIMIT 4",
    );
    assert_eq!(
        d,
        vec![
            (1, Some(0.0)),
            (2, Some(5.0)),
            (3, Some(10.0)),
            (4, Some(50.0))
        ]
    );
}

/// 未対応の形は fail-closed（コードを固定する）。
#[test]
fn unsupported_forms_are_rejected() {
    let (_g, core) = open("ivd-reject");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), body TEXT)",
    );
    ok(
        &core,
        "INSERT INTO t (id, v, body) VALUES (1, '[1,2]', 'a')",
    );
    let cases: &[(&str, &str)] = &[
        // L1 は ORDER BY・投影・WHERE のいずれでも 0A000。
        ("SELECT id FROM t ORDER BY v <+> '[1,2]' LIMIT 1", "0A000"),
        // 投影の距離列は ORDER BY と同じ演算子でなければならない（構文段の 42601）。
        (
            "SELECT id, v <+> '[1,2]' AS d FROM t ORDER BY id LIMIT 1",
            "42601",
        ),
        (
            "SELECT id FROM t WHERE v <+> '[1,2]' < 3 ORDER BY id LIMIT 1",
            "0A000",
        ),
        // hybrid・USING PLAN・precision・HINT ORDER。
        (
            "SELECT id FROM t ORDER BY hybrid_rrf(v, '[1,2]', body, 'q') LIMIT 1",
            "0A000",
        ),
        ("SELECT id FROM t USING PLAN('find a') LIMIT 1", "0A000"),
        (
            "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 1 USING MODE 'precision'",
            "0A000",
        ),
        (
            "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 1 USING MODE 'recall'",
            "0A000",
        ),
        (
            "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 1 HINT ORDER(RLS, DISTANCE, SCALAR)",
            "0A000",
        ),
        // 距離関数形は INT8VEC 非対応。
        ("SELECT l2_distance(v, '[1,2]') FROM t", "22000"),
        // リテラルの検証。
        ("SELECT id FROM t ORDER BY v <-> '[1.5,2]' LIMIT 1", "22P02"),
        ("SELECT id FROM t ORDER BY v <-> '[1,abc]' LIMIT 1", "22P02"),
        ("SELECT id FROM t ORDER BY v <-> '[1,200]' LIMIT 1", "22003"),
        ("SELECT id FROM t ORDER BY v <-> '[1,2,3]' LIMIT 1", "22000"),
        // 投影の距離列のクエリが ORDER BY と違う。
        (
            "SELECT id, v <-> '[9,9]' AS d FROM t ORDER BY v <-> '[1,2]' LIMIT 1",
            "0A000",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(code(&core, sql), *expected, "{sql}");
    }
    // precision はセッション設定でも拒否する。
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for("alice"), &mut s, "SET search_mode = 'precision'")
        .expect("set search_mode");
    let err = core
        .execute_sql_in_session(
            &ctx_for("alice"),
            &mut s,
            "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 1",
        )
        .expect_err("precision must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

/// 距離式を別の列型へ向けた混在は従来どおり拒否する（VECTOR 側の契約を壊さない）。
#[test]
fn mixed_column_types_do_not_regress() {
    let (_g, core) = open("ivd-mixed");
    ok(
        &core,
        "CREATE TABLE tv (id BIGINT PRIMARY KEY, e VECTOR(2), n INTEGER)",
    );
    ok(&core, "INSERT INTO tv (id, e, n) VALUES (1, '[1,2]', 1)");
    // VECTOR の `<->` 演算子形 kNN は従来どおり 0A000（INT8VEC との非対称。docs に記録）。
    assert_eq!(
        code(&core, "SELECT id FROM tv ORDER BY e <-> '[1,2]' LIMIT 1"),
        "0A000"
    );
    // INT8VEC でない列への距離式は 42883。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM tv WHERE n <-> '[1,2]' < 3 ORDER BY id LIMIT 1"
        ),
        "42883"
    );
    // VECTOR の cosine kNN は従来どおり動く。
    assert_eq!(
        ids(&core, "SELECT id FROM tv ORDER BY e <=> '[1,2]' LIMIT 1"),
        vec![1]
    );
}

/// EXPLAIN は scan の計画を返す。
#[test]
fn explain_reports_scan_plan() {
    let (_g, core, _rows) = setup("ivd-explain", 4, 5, 7);
    let SqlOutcome::Explain(q) = exec_as(
        &core,
        "alice",
        "EXPLAIN SELECT id FROM t ORDER BY v <-> '[1,2,3,4]' LIMIT 3",
    )
    .expect("explain") else {
        panic!("EXPLAIN must return a plan");
    };
    let text: Vec<String> = q
        .rows
        .iter()
        .flat_map(|r| r.cells.iter())
        .filter_map(|c| match c {
            Cell::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let joined = text.join("\n");
    assert!(joined.contains("plain_scan"), "{joined}");
}

/// テナント境界: 同じ id・近い値の行が他テナントにあっても結果・エラーへ出ない。
#[test]
fn tenant_isolation_holds_for_knn() {
    let (_g, core) = open("ivd-tenant");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok_as(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (1, '[50,50]')",
    );
    ok_as(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (2, '[60,60]')",
    );
    // bob の行は alice のクエリに最も近い（出てはならない）。
    ok_as(&core, "bob", "INSERT INTO t (id, v) VALUES (11, '[0,0]')");
    ok_as(&core, "bob", "INSERT INTO t (id, v) VALUES (12, '[1,1]')");
    for op in Op::ALL {
        let sql = format!("SELECT id FROM t ORDER BY v {} '[0,0]' LIMIT 10", op.sql());
        assert_eq!(ids_as(&core, "alice", &sql).len(), 2, "{op:?}");
        assert!(
            ids_as(&core, "alice", &sql).iter().all(|i| *i < 10),
            "{op:?}"
        );
        assert!(ids_as(&core, "bob", &sql).iter().all(|i| *i > 10), "{op:?}");
    }
    // 半径検索（WHERE）でも他テナントの行は出ない。
    assert_eq!(
        ids_as(
            &core,
            "alice",
            "SELECT id FROM t WHERE v <-> '[0,0]' < 1000 ORDER BY id LIMIT 10"
        ),
        vec![1, 2]
    );
    // 拒否のエラー文に他テナントの値が現れない。
    let err = exec_as(
        &core,
        "alice",
        "SELECT id FROM t ORDER BY v <+> '[0,0]' LIMIT 1",
    )
    .expect_err("L1 rejected");
    assert!(!format!("{err:?}").contains("bob"));
}

/// 行ポリシー（RLS-13）: ポリシーで隠れた行は kNN の結果にも距離の順位にも出ない
/// （書き換え後の scan 経路がポリシー段を通る）。
#[test]
fn row_policy_hides_rows_from_knn() {
    let (_g, core) = open("ivd-policy");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), n INTEGER)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '[0,0]', 0)");
    ok(&core, "INSERT INTO t (id, v, n) VALUES (2, '[1,1]', 1)");
    ok(&core, "INSERT INTO t (id, v, n) VALUES (3, '[2,2]', 1)");
    ok(&core, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ok(&core, "ALTER TABLE t FORCE ROW LEVEL SECURITY");
    ok(&core, "CREATE POLICY p ON t FOR SELECT USING (n = 1)");
    for op in Op::ALL {
        let sql = format!("SELECT id FROM t ORDER BY v {} '[0,0]' LIMIT 10", op.sql());
        let got = ids(&core, &sql);
        assert!(!got.contains(&1), "{op:?}: hidden row leaked: {got:?}");
        assert_eq!(got.len(), 2, "{op:?}");
    }
}

// --- f32 比の誤差の実測（TABLE-49 (5)。値は docs/design/column-type-extension.md に転記） -------

/// `vector` 表の参照クエリ。L2 は演算子形 kNN が未対応のため関数形（`l2_distance`）を使う。
fn vector_side_sql(op: Op, q: &[i8], limit: usize) -> String {
    let expr = match op {
        Op::L2 => format!("l2_distance(e, '{}')", lit(q)),
        Op::Ip => format!("e <#> '{}'", lit(q)),
        Op::Cos => format!("e <=> '{}'", lit(q)),
    };
    format!("SELECT id, {expr} AS d FROM tv ORDER BY {expr} LIMIT {limit}")
}

fn int8_side_sql(op: Op, q: &[i8], limit: usize) -> String {
    let expr = format!("v {} '{}'", op.sql(), lit(q));
    format!("SELECT id, {expr} AS d FROM t ORDER BY {expr} LIMIT {limit}")
}

/// 2 つの距離列（id 付き）の最大絶対誤差と最大相対誤差（f32 側を参照とし、0 と NaN は相対から除く）。
fn error_stats(int8: &[(i64, Option<f64>)], f32s: &[(i64, Option<f64>)]) -> (f64, f64) {
    let by_id: std::collections::HashMap<i64, f64> = f32s
        .iter()
        .filter_map(|(i, d)| d.map(|d| (*i, d)))
        .collect();
    let mut max_abs = 0.0f64;
    let mut max_rel = 0.0f64;
    for (id, d) in int8 {
        let (Some(d), Some(r)) = (d, by_id.get(id)) else {
            continue;
        };
        if d.is_nan() || r.is_nan() {
            continue;
        }
        let abs = (d - r).abs();
        max_abs = max_abs.max(abs);
        if r.abs() > 0.0 {
            max_rel = max_rel.max(abs / r.abs());
        }
    }
    (max_abs, max_rel)
}

/// M1: 同じ整数値を持つ `vector` 表との比較。Top-k の id 列が一致し、距離値の差は f32 の丸め誤差の
/// 範囲に収まる（INT8VEC 側は整数のまま厳密。差は f32 側の丸めで、極端な fixture ほど大きい）。
#[test]
fn m1_int8vec_matches_vector_with_same_values() {
    for dim in [8usize, 128, 1536] {
        let (_g, core) = open(&format!("ivd-m1-{dim}"));
        ok(
            &core,
            &format!("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC({dim}))"),
        );
        ok(
            &core,
            &format!("CREATE TABLE tv (id BIGINT PRIMARY KEY, e VECTOR({dim}))"),
        );
        let rows = make_rows(0xabcd_ef01_2345_6789 ^ dim as u64, 40, dim);
        insert_rows(&core, "t", &rows);
        for (id, v) in &rows {
            ok(
                &core,
                &format!("INSERT INTO tv (id, e) VALUES ({id}, '{}')", lit(v)),
            );
        }
        let q = make_rows(0x1357_9bdf_0246_8ace ^ dim as u64, 1, dim)
            .remove(0)
            .1;
        for op in Op::ALL {
            let a = id_dists(&core, &int8_side_sql(op, &q, 40));
            let b = id_dists(&core, &vector_side_sql(op, &q, 40));
            let top_a: Vec<i64> = a.iter().take(10).map(|r| r.0).collect();
            let top_b: Vec<i64> = b.iter().take(10).map(|r| r.0).collect();
            assert_eq!(top_a, top_b, "dim {dim} {op:?}: top-10 must agree");
            let (abs, rel) = error_stats(&a, &b);
            eprintln!("M1 dim={dim} op={op:?} max_abs_err={abs:e} max_rel_err={rel:e}");
            assert!(rel < 1e-4, "dim {dim} {op:?}: relative error {rel:e}");
            // 整数のまま厳密な参照との一致（INT8VEC 側）。
            let want = ref_top(op, &rows, &q, 40);
            for ((gid, gd), (wid, wd)) in a.iter().zip(&want) {
                assert_eq!(gid, wid);
                assert_eq!(gd.expect("d"), *wd);
            }
        }
    }
    // 極端な fixture: 全要素 ±127・dim 1536。f32 の累積が 2^24 を超えて丸めが出うる範囲で、
    // INT8VEC は厳密、f32 側がずれることを記録する（順位の一致は主張しない）。
    let dim = 1536usize;
    let (_g, core) = open("ivd-m1-extreme");
    ok(
        &core,
        &format!("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC({dim}))"),
    );
    ok(
        &core,
        &format!("CREATE TABLE tv (id BIGINT PRIMARY KEY, e VECTOR({dim}))"),
    );
    let mut rng = Rng(0x0f0f_f0f0_1234_4321);
    let mut rows: Vec<(i64, Vec<i8>)> = (1..=20)
        .map(|id| {
            (
                id,
                (0..dim)
                    .map(|_| if rng.next() & 1 == 0 { 127 } else { -127 })
                    .collect(),
            )
        })
        .collect();
    // 内積が 2^24 を超える奇数になる行（全要素 127 で末尾だけ 126）。f32 では奇数を表せない。
    let mut odd = vec![127i8; dim];
    odd[dim - 1] = 126;
    rows.push((21, odd));
    insert_rows(&core, "t", &rows);
    for (id, v) in &rows {
        ok(
            &core,
            &format!("INSERT INTO tv (id, e) VALUES ({id}, '{}')", lit(v)),
        );
    }
    let q = vec![127i8; dim];
    for op in Op::ALL {
        let a = id_dists(&core, &int8_side_sql(op, &q, 21));
        let b = id_dists(&core, &vector_side_sql(op, &q, 21));
        let (abs, rel) = error_stats(&a, &b);
        eprintln!("M1 extreme dim={dim} op={op:?} max_abs_err={abs:e} max_rel_err={rel:e}");
        let want = ref_top(op, &rows, &q, 21);
        for ((gid, gd), (wid, wd)) in a.iter().zip(&want) {
            assert_eq!(gid, wid);
            assert_eq!(gd.expect("d"), *wd, "INT8VEC side must stay exact");
        }
    }
}

/// M2: f32 データを表全体で 1 つの対称スケールで int8 へ量子化したときの誤差。f32 の厳密走査
/// （f64）を参照に、3 演算子それぞれの recall@10 とスケールを戻した距離の相対誤差を記録する。
#[test]
fn m2_quantization_error_against_f32_reference() {
    const N: usize = 500;
    const D: usize = 64;
    const Q: usize = 20;
    let mut rng = Rng(0x5eed_0000_cafe_f00d);
    let mut gen = |n: usize| -> Vec<Vec<f64>> {
        (0..n)
            .map(|_| {
                (0..D)
                    .map(|_| f64::from(((rng.next() >> 40) as f32) / 8_388_608.0 - 1.0))
                    .collect()
            })
            .collect()
    };
    let base = gen(N);
    let queries = gen(Q);
    let max_abs = base
        .iter()
        .chain(&queries)
        .flatten()
        .fold(0.0f64, |m, x| m.max(x.abs()));
    let scale = 127.0 / max_abs;
    let quant = |v: &Vec<f64>| -> Vec<i8> { v.iter().map(|x| (x * scale).round() as i8).collect() };

    let (_g, core) = open("ivd-m2");
    ok(
        &core,
        &format!("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC({D}))"),
    );
    let qrows: Vec<(i64, Vec<i8>)> = base
        .iter()
        .enumerate()
        .map(|(i, v)| (i as i64 + 1, quant(v)))
        .collect();
    insert_rows(&core, "t", &qrows);

    let f64_dist = |op: Op, a: &[f64], q: &[f64]| -> f64 {
        let d: f64 = a.iter().zip(q).map(|(x, y)| x * y).sum();
        let na: f64 = a.iter().map(|x| x * x).sum();
        let nq: f64 = q.iter().map(|x| x * x).sum();
        match op {
            Op::L2 => (na + nq - 2.0 * d).max(0.0).sqrt(),
            Op::Ip => -d,
            Op::Cos => 1.0 - (d / (na * nq).sqrt()).clamp(-1.0, 1.0),
        }
    };

    for op in Op::ALL {
        let mut recalls = Vec::new();
        let mut max_rel = 0.0f64;
        let mut sum_rel = 0.0f64;
        let mut cnt_rel = 0usize;
        for qv in &queries {
            let qq = quant(qv);
            // 参照: 元の f32（f64 で厳密走査）の Top-10。
            let mut truth: Vec<(i64, f64)> = base
                .iter()
                .enumerate()
                .map(|(i, v)| (i as i64 + 1, f64_dist(op, v, qv)))
                .collect();
            truth.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            let truth_ids: std::collections::HashSet<i64> =
                truth.iter().take(10).map(|t| t.0).collect();
            let truth_by_id: std::collections::HashMap<i64, f64> = truth.iter().copied().collect();
            let got = id_dists(&core, &int8_side_sql(op, &qq, N));
            let hit = got
                .iter()
                .take(10)
                .filter(|(id, _)| truth_ids.contains(id))
                .count();
            recalls.push(hit as f64 / 10.0);
            for (id, d) in &got {
                let d = d.expect("d");
                let t = truth_by_id[id];
                let restored = match op {
                    Op::L2 => d / scale,
                    Op::Ip => d / (scale * scale),
                    Op::Cos => d,
                };
                if t.abs() > 1e-9 {
                    let rel = (restored - t).abs() / t.abs();
                    max_rel = max_rel.max(rel);
                    sum_rel += rel;
                    cnt_rel += 1;
                }
            }
        }
        let mean_recall = recalls.iter().sum::<f64>() / recalls.len() as f64;
        let min_recall = recalls.iter().copied().fold(1.0f64, f64::min);
        eprintln!(
            "M2 op={op:?} recall@10 mean={mean_recall:.4} min={min_recall:.2} \
             mean_rel_err={:.3e} max_rel_err={max_rel:.3e}",
            sum_rel / cnt_rel as f64
        );
        assert!(mean_recall >= 0.7, "{op:?}: recall@10 mean {mean_recall}");
    }
}

/// Issue #2953 レビュー指摘: 明示 LIMIT + OFFSET が結果行数上限を超える INT8VEC kNN は
/// Execute・Describe・EXPLAIN のすべてで `54000` で拒否し、上限ちょうどは受理する。
#[test]
fn limit_plus_offset_over_result_row_limit_fails_closed() {
    let (_g, core) = open("ivd-limit-total");
    let core = core.with_result_limits(engine::result_limits::ResultLimits::new(10));
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    let over = "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 10 OFFSET 1";
    assert_eq!(code(&core, over), "54000");
    assert_eq!(code(&core, &format!("EXPLAIN {over}")), "54000");
    let session = SessionState::default();
    let prepared = core.parse_sql_prepared(over).expect("prepare");
    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err("describe must reject");
    assert_eq!(err.wire_code(), "54000");
    ok(
        &core,
        "SELECT id FROM t ORDER BY v <=> '[1,2]' LIMIT 9 OFFSET 1",
    );
}

/// Issue #2953 レビュー指摘: Describe の検証省略は `$n` 由来の距離式だけに限る。WHERE・投影の
/// 実リテラル（範囲外・次元不一致）は `$n` が ORDER BY にあっても Describe で拒否する。
#[test]
fn describe_validates_real_literals_next_to_placeholders() {
    let (_g, core) = open("ivd-describe-real");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    let describe = |sql: &str| {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("prepare");
        core.describe_prepared_in_session(&session, &prepared)
    };
    describe("SELECT id FROM t ORDER BY v <=> $1 LIMIT 3").expect("placeholder describe");
    describe("SELECT id, v <-> $1 FROM t ORDER BY v <-> $2 LIMIT 3")
        .expect("projection placeholder describe");
    for (sql, want) in [
        (
            "SELECT id FROM t WHERE v <-> '[1,200]' < 5 ORDER BY v <=> $1 LIMIT 3",
            "22003",
        ),
        (
            "SELECT id FROM t WHERE v <-> '[1,2,3]' < 5 ORDER BY v <=> $1 LIMIT 3",
            "22000",
        ),
        (
            "SELECT id, v <-> '[1,x]' FROM t ORDER BY v <-> $1 LIMIT 3",
            "22P02",
        ),
        ("SELECT id FROM t ORDER BY v <=> '[1,200]' LIMIT 3", "22003"),
    ] {
        let err = describe(sql).expect_err(sql);
        assert_eq!(err.wire_code(), want, "{sql}");
    }
}

/// Issue #2953 Bugbot 指摘: WHERE の半径述語（`v <-> $n < r`）に `$n` を持つ INT8VEC kNN も
/// Parse → Describe が通る（`$n` 由来のダミー値を距離リテラルとして解析しない）。
/// 同じ文の実リテラルは従来どおり Describe で検証する。
#[test]
fn describe_accepts_where_radius_placeholder() {
    let (_g, core) = open("ivd-describe-where");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    let describe = |sql: &str| {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("prepare");
        core.describe_prepared_in_session(&session, &prepared)
    };
    for sql in [
        "SELECT id FROM t WHERE v <-> $1 < 5 ORDER BY v <=> '[1,2]' LIMIT 3",
        "SELECT id FROM t WHERE v <-> $1 < 5 ORDER BY v <=> $2 LIMIT 3",
        "SELECT id, v <-> $1 FROM t WHERE v <-> $2 < 5 ORDER BY v <-> $3 LIMIT 3",
    ] {
        describe(sql).unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
    }
    let err = describe(
        "SELECT id FROM t WHERE v <-> $1 < 5 AND v <-> '[1,200]' < 5 ORDER BY v <=> $2 LIMIT 3",
    )
    .expect_err("real literal");
    assert_eq!(err.wire_code(), "22003");
}
/// Issue #2953 PR #2956 指摘: BETWEEN／IN の脱糖で左辺の距離式が複製されても、`$n` 由来の仮値を
/// すべての複製で置換し、Describe が通る。
#[test]
fn describe_accepts_placeholder_in_between_and_in() {
    let (_g, core) = open("ivd-describe-between-in");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    let describe = |sql: &str| {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("prepare");
        core.describe_prepared_in_session(&session, &prepared)
    };
    for sql in [
        "SELECT id FROM t WHERE v <-> $1 BETWEEN 0 AND 5 ORDER BY id LIMIT 3",
        "SELECT id FROM t WHERE v <-> $1 NOT BETWEEN 0 AND 5 ORDER BY id LIMIT 3",
        "SELECT id FROM t WHERE v <-> $1 IN (1, 2, 3) ORDER BY id LIMIT 3",
    ] {
        describe(sql).unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
    }
}

/// Issue #2953 PR #2956 指摘: 実リテラル `'0'` を `$n` の仮値と取り違えて置換しない。`$n` 由来の距離式が
/// 同じ文にあっても、`'0'` は Describe で `22P02` になる（Execute と同じ契約）。
#[test]
fn describe_rejects_real_zero_literal_next_to_placeholder() {
    let (_g, core) = open("ivd-describe-real-zero");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    let describe = |sql: &str| {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("prepare");
        core.describe_prepared_in_session(&session, &prepared)
    };
    for sql in [
        "SELECT id FROM t WHERE v <-> $1 < 5 AND v <-> '0' < 5 ORDER BY v <=> '[1,2]' LIMIT 3",
        "SELECT id FROM t WHERE v <-> '0' < 5 AND v <-> $1 < 5 ORDER BY v <=> '[1,2]' LIMIT 3",
        "SELECT id FROM t WHERE v <-> $1 < 5 AND v <-> '0' < 5 ORDER BY id LIMIT 3",
    ] {
        let err = describe(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "22P02", "{sql}");
    }
}

/// Issue #2953 PR #2956 指摘: ORDER BY が距離でない Scan 形の半径検索も、Describe が `$n` 由来の距離式を
/// 通し、有効な値を Bind して Execute できる。同じ文の実リテラルは Describe で検証される。
#[test]
fn scan_form_radius_placeholder_describes_and_executes() {
    let (_g, core) = open("ivd-scan-radius-param");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    ok(&core, "INSERT INTO t (id, v) VALUES (2, '[100,100]')");
    let session = SessionState::default();
    let sql = "SELECT id FROM t WHERE v <-> $1 < 5 ORDER BY id LIMIT 3";
    let prepared = core.parse_sql_prepared(sql).expect("prepare");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("scan-form describe");
    let parsed = core
        .bind_prepared(&prepared, &[Some(b"[1,2]".to_vec())])
        .expect("bind");
    let mut session = SessionState::default();
    let out = core
        .execute_parsed_in_session(&ctx_for("alice"), &mut session, &parsed)
        .expect("execute");
    let SqlOutcome::Query(q) = out else {
        panic!("expected query outcome");
    };
    assert_eq!(q.rows.len(), 1);
    let err = core
        .parse_sql_prepared("SELECT id FROM t WHERE v <-> $1 < 5 AND v <-> '0' < 5 ORDER BY id")
        .and_then(|p| core.describe_prepared_in_session(&session, &p))
        .expect_err("real literal");
    assert_eq!(err.wire_code(), "22P02");
}
