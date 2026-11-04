//! `HALFVEC(N)` 列の距離計算（4 metric）と kNN、`'[..]'::halfvec` のテキスト入出力
//! （TABLE-46・Issue #2015。距離関数は SQL-72、距離演算子は SQL-33、WHERE の距離式は SQL-74 のポインタ）の
//! 結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で次を固定する。
//! (1) f16 で厳密に表せる値では VECTOR 表と HALFVEC 表の距離・Top-k がビット一致する（f16 を f32 へ
//!     広げて同じ f32 カーネルで計算するため）。
//! (2) 疑似乱数 fixture では HALFVEC の Top-k が f32 参照と「f16 丸め誤差から導いた許容誤差内」で一致する
//!     （精度の実測値は docs/design/column-type-extension.md に記録する）。
//! (3) 並びの境界（NULL・ゼロノルムの NaN・同点・OFFSET・DESC）、(4) テキスト入出力、
//! (5) 問い合わせ側の f16 丸め、(6) 未対応形の fail-closed、(7) テナント境界、(8) Describe の一致。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
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

fn ok(core: &EngineCore, sql: &str) {
    exec_as(core, "alice", sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec_as(core, "alice", sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn query_cells(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    let SqlOutcome::Query(q) =
        exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    else {
        panic!("{sql} must return a query result");
    };
    q.rows.into_iter().map(|r| r.cells).collect()
}

fn cell_id(c: &Cell) -> i64 {
    match c {
        Cell::SignedInteger(n) => *n,
        Cell::Integer(n) => i64::try_from(*n).expect("id"),
        other => panic!("unexpected id cell {other:?}"),
    }
}

fn ids(core: &EngineCore, sql: &str) -> Vec<i64> {
    query_cells(core, "alice", sql)
        .iter()
        .map(|r| cell_id(&r[0]))
        .collect()
}

/// `SELECT id, <distance> ...` を `(id, 距離)` で返す（NULL は `None`）。
fn dists(core: &EngineCore, sql: &str) -> Vec<(i64, Option<f64>)> {
    query_cells(core, "alice", sql)
        .iter()
        .map(|r| {
            let d = match &r[1] {
                Cell::Float(f) => Some(*f),
                Cell::Null => None,
                other => panic!("unexpected distance cell {other:?}"),
            };
            (cell_id(&r[0]), d)
        })
        .collect()
}

fn text(core: &EngineCore, sql: &str) -> String {
    match &query_cells(core, "alice", sql)[0][0] {
        Cell::Text(s) => s.clone(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn lit(v: &[f32]) -> String {
    let body: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", body.join(","))
}

/// 線形合同法（依存なし）。`[-1, 1]` の f32 を返す。
struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let top = (self.0 >> 40) as u32; // 24 bit
        (top as f32 / 8_388_608.0) - 1.0
    }
}

fn seed_pair(core: &EngineCore, rows: &[Vec<f32>], dim: usize) {
    ok(
        core,
        &format!("CREATE TABLE rv (id BIGINT PRIMARY KEY, v VECTOR({dim}))"),
    );
    ok(
        core,
        &format!("CREATE TABLE hv (id BIGINT PRIMARY KEY, h HALFVEC({dim}))"),
    );
    for chunk in rows.chunks(50).enumerate() {
        let (ci, chunk) = chunk;
        let values: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| format!("({}, '{}')", ci * 50 + i, lit(v)))
            .collect();
        ok(
            core,
            &format!("INSERT INTO rv (id, v) VALUES {}", values.join(",")),
        );
        ok(
            core,
            &format!("INSERT INTO hv (id, h) VALUES {}", values.join(",")),
        );
    }
}

/// (1) f16 で厳密に表せる値では、4 metric の関数形・演算子形の kNN がビット一致する。
#[test]
fn exact_values_match_vector_bit_for_bit() {
    let (_g, core) = open("hvd-exact");
    // 0.5 刻みの値は f16 で厳密に表せる。
    let mut rng = Lcg(7);
    let rows: Vec<Vec<f32>> = (0..40)
        .map(|_| {
            (0..6)
                .map(|_| (rng.next_f32() * 8.0).round() / 2.0)
                .collect()
        })
        .collect();
    seed_pair(&core, &rows, 6);
    let q = "[1.5,-2,0.5,3,0,-1]";
    for (rf, hf) in [
        ("l2_distance(v, ", "l2_distance(h, "),
        ("cosine_distance(v, ", "cosine_distance(h, "),
        ("inner_product(v, ", "inner_product(h, "),
        ("l1_distance(v, ", "l1_distance(h, "),
    ] {
        let r = dists(&core, &format!("SELECT id, {rf}'{q}') FROM rv ORDER BY id"));
        let h = dists(&core, &format!("SELECT id, {hf}'{q}') FROM hv ORDER BY id"));
        assert_eq!(r.len(), h.len());
        for ((ri, rd), (hi, hd)) in r.iter().zip(h.iter()) {
            assert_eq!(ri, hi);
            assert_eq!(
                rd.map(f64::to_bits),
                hd.map(f64::to_bits),
                "{hf} id {hi}: HALFVEC distance must equal the VECTOR distance bit for bit"
            );
        }
        // 関数形 ORDER BY の Top-k。
        let rk = ids(
            &core,
            &format!("SELECT id FROM rv ORDER BY {rf}'{q}') LIMIT 7"),
        );
        let hk = ids(
            &core,
            &format!("SELECT id FROM hv ORDER BY {hf}'{q}') LIMIT 7"),
        );
        assert_eq!(rk, hk, "{hf}: Top-k ids");
    }
    // 演算子形（HALFVEC は scan 経由、VECTOR は既存の kNN 経路）。
    for op in ["<=>", "<#>"] {
        for desc in ["", " DESC"] {
            let rk = ids(
                &core,
                &format!("SELECT id FROM rv ORDER BY v {op} '{q}'{desc} LIMIT 9"),
            );
            let hk = ids(
                &core,
                &format!("SELECT id FROM hv ORDER BY h {op} '{q}'{desc} LIMIT 9"),
            );
            assert_eq!(rk, hk, "{op}{desc}: Top-k ids");
        }
    }
    // WHERE の距離式（SQL-74）と投影。
    let r = ids(
        &core,
        &format!("SELECT id FROM rv WHERE v <-> '{q}' < 4 ORDER BY id"),
    );
    let h = ids(
        &core,
        &format!("SELECT id FROM hv WHERE h <-> '{q}' < 4 ORDER BY id"),
    );
    assert_eq!(r, h);
    assert!(!h.is_empty());
}

/// 疑似乱数 fixture の許容誤差内一致。値は f16 に丸まるため、f32 参照との距離の差は f16 丸め誤差
/// （正規化数で相対 2^-11、サブノーマルで絶対 2^-25）から導いた上限以内に収まる。
#[test]
fn random_fixture_top_k_matches_f32_reference_within_tolerance() {
    const DIM: usize = 32;
    const N: usize = 200;
    const K: usize = 10;
    let (_g, core) = open("hvd-random");
    let mut rng = Lcg(20_240_229);
    let rows: Vec<Vec<f32>> = (0..N)
        .map(|_| (0..DIM).map(|_| rng.next_f32()).collect())
        .collect();
    seed_pair(&core, &rows, DIM);
    let eps = 2f64.powi(-11) + 2f64.powi(-25);
    let e_norm = (DIM as f64).sqrt() * eps; // 1 ベクトルの丸め誤差ベクトルのノルム上限
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let min_row_norm = rows.iter().map(|r| norm(r)).fold(f64::MAX, f64::min);

    // (名前, 参照/HALFVEC の距離式, HALFVEC の ORDER BY 式, 符号, 許容誤差)
    let l2_tol = 2.0 * e_norm + 1e-5;
    let l1_tol = 2.0 * (DIM as f64) * eps + 1e-4;
    let ip_tol = (DIM as f64) * (2.0 * eps + eps * eps) + 1e-4;
    let mut report = String::new();
    let mut min_recall = 1.0f64;
    for qi in 0..8 {
        let q: Vec<f32> = (0..DIM).map(|_| rng.next_f32()).collect();
        let ql = lit(&q);
        let cos_tol = 2.0 * (e_norm / min_row_norm + e_norm / norm(&q)) + 1e-5;
        let metrics: [(&str, &str, &str, f64, f64); 4] = [
            (
                "l2",
                "l2_distance",
                "ORDER BY l2_distance(h, '{q}')",
                1.0,
                l2_tol,
            ),
            (
                "cosine",
                "cosine_distance",
                "ORDER BY h <=> '{q}'",
                1.0,
                cos_tol,
            ),
            ("ip", "inner_product", "ORDER BY h <#> '{q}'", -1.0, ip_tol),
            (
                "l1",
                "l1_distance",
                "ORDER BY l1_distance(h, '{q}')",
                1.0,
                l1_tol,
            ),
        ];
        for (name, func, order, sign, tol) in metrics {
            let reference: std::collections::HashMap<i64, f64> =
                dists(&core, &format!("SELECT id, {func}(v, '{ql}') FROM rv"))
                    .into_iter()
                    .map(|(i, d)| (i, sign * d.expect("non-null")))
                    .collect();
            let halves: std::collections::HashMap<i64, f64> =
                dists(&core, &format!("SELECT id, {func}(h, '{ql}') FROM hv"))
                    .into_iter()
                    .map(|(i, d)| (i, sign * d.expect("non-null")))
                    .collect();
            let (mut max_abs, mut max_rel) = (0.0f64, 0.0f64);
            for (id, rd) in &reference {
                let hd = halves[id];
                let abs = (hd - rd).abs();
                max_abs = max_abs.max(abs);
                if rd.abs() > 1e-3 {
                    max_rel = max_rel.max(abs / rd.abs());
                }
                assert!(abs <= tol, "{name} q{qi} id {id}: |{hd} - {rd}| > {tol}");
            }
            let order_sql = order.replace("{q}", &ql);
            let topk = ids(&core, &format!("SELECT id FROM hv {order_sql} LIMIT {K}"));
            assert_eq!(topk.len(), K);
            // f32 参照の Top-k（参照の距離を昇順に並べる。同点は id 昇順）。
            let mut sorted: Vec<(i64, f64)> = reference.iter().map(|(i, d)| (*i, *d)).collect();
            sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            let d_k = sorted[K - 1].1;
            for id in &topk {
                assert!(
                    reference[id] <= d_k + 2.0 * tol,
                    "{name} q{qi}: id {id} is not within 2*tol of the f32 k-th distance"
                );
            }
            let ref_set: std::collections::HashSet<i64> =
                sorted.iter().take(K).map(|(i, _)| *i).collect();
            let hit = topk.iter().filter(|i| ref_set.contains(i)).count();
            let recall = hit as f64 / K as f64;
            min_recall = min_recall.min(recall);
            report.push_str(&format!(
                "q{qi} {name}: recall@{K}={recall:.2} max_abs_err={max_abs:.3e} max_rel_err={max_rel:.3e}\n"
            ));
        }
    }
    eprintln!("{report}");
    assert!(min_recall >= 0.5, "recall collapsed: {min_recall}");
}

/// (3) 並びの境界。NULL は ASC で末尾・DESC で先頭、ゼロノルムのコサイン NaN は NULL を除いた末尾、
/// 同点は id 昇順。LIMIT 省略・OFFSET も VECTOR の kNN と同じ契約で動く。
#[test]
fn ordering_boundaries() {
    let (_g, core) = open("hvd-order");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(2))",
    );
    ok(
        &core,
        "INSERT INTO t (id, h) VALUES (1, '[1,0]'), (2, '[0,0]'), (3, NULL), (4, '[1,0]'), \
         (5, '[0,1]')",
    );
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY h <=> '[1,0]' LIMIT 10"),
        vec![1, 4, 5, 2, 3]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY h <=> '[1,0]' DESC LIMIT 10"
        ),
        vec![3, 2, 5, 1, 4]
    );
    // LIMIT 省略（暗黙上限）。
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY h <=> '[1,0]'"),
        vec![1, 4, 5, 2, 3]
    );
    // OFFSET。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY h <=> '[1,0]' LIMIT 2 OFFSET 1"
        ),
        vec![4, 5]
    );
    // WHERE との併用と投影される距離。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE id > 1 ORDER BY h <#> '[1,0]' LIMIT 10"
        ),
        vec![4, 2, 5, 3]
    );
    let rows = query_cells(
        &core,
        "alice",
        "SELECT id, h <=> '[1,0]' FROM t ORDER BY h <=> '[1,0]' LIMIT 2",
    );
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][1], Cell::Float(d) if d == 0.0));
}

/// (4) テキスト入出力。丸めは最近接偶数、出力は f16 を f32 へ広げた最短表記。
#[test]
fn text_input_and_output() {
    let (_g, core) = open("hvd-text");
    assert_eq!(
        text(&core, "SELECT '[0.1,1,-2.5]'::halfvec"),
        "[0.099975586,1,-2.5]"
    );
    assert_eq!(text(&core, "SELECT '[0.1]'::halfvec(1)"), "[0.099975586]");
    assert_eq!(
        text(&core, "SELECT CAST('[0.1,2]' AS halfvec(2))"),
        "[0.099975586,2]"
    );
    assert_eq!(text(&core, "SELECT halfvec '[0.1,2]'"), "[0.099975586,2]");
    assert_eq!(text(&core, "SELECT '[65519]'::halfvec"), "[65504]");
    assert_eq!(code(&core, "SELECT '[70000]'::halfvec"), "22003");
    assert_eq!(code(&core, "SELECT '[1,2]'::halfvec(3)"), "22000");
    assert_eq!(code(&core, "SELECT '[1,2'::halfvec"), "22000");
    assert_eq!(code(&core, "SELECT '[]'::halfvec"), "22000");
    assert_eq!(code(&core, "SELECT '[1,2]'::halfvec(0)"), "22000");
    // 列値の取り出しと同じ表記になる。
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(1))",
    );
    ok(&core, "INSERT INTO t (id, h) VALUES (1, '[0.1]')");
    assert_eq!(
        query_cells(&core, "alice", "SELECT h FROM t")[0][0],
        Cell::Vector(vec![0.099_975_586])
    );
}

/// (5) 問い合わせ側も f16 に丸める（pgvector は型なしリテラルを halfvec 列の相手として halfvec に解決する）。
#[test]
fn query_is_rounded_to_f16() {
    let (_g, core) = open("hvd-qround");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(1))",
    );
    ok(&core, "INSERT INTO t (id, h) VALUES (1, '[0]')");
    let rounded = f64::from(0.099_975_586_f32);
    let unrounded = f64::from(0.1_f32);
    assert_ne!(rounded, unrounded);
    for q in ["'[0.1]'", "'[0.1]'::halfvec"] {
        let d = dists(&core, &format!("SELECT id, l2_distance(h, {q}) FROM t"));
        assert_eq!(d[0].1, Some(rounded), "{q}");
        let w = ids(
            &core,
            &format!("SELECT id FROM t WHERE h <-> {q} = {rounded:?}"),
        );
        assert_eq!(w, vec![1], "{q}");
    }
    let d = dists(&core, "SELECT id, l1_distance('[0.1]', h) FROM t");
    assert_eq!(d[0].1, Some(rounded));
}

/// (6) 未対応の形は fail-closed。どのエラーにもリテラルの値を含めない。
#[test]
fn unsupported_forms_fail_closed() {
    let (_g, core) = open("hvd-closed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(2), n INTEGER)",
    );
    ok(&core, "CREATE TABLE r (id BIGINT PRIMARY KEY, v VECTOR(2))");
    ok(&core, "INSERT INTO t (id, h, n) VALUES (1, '[1,2]', 1)");
    let cases: &[(&str, &str)] = &[
        // L2／L1 の演算子形 kNN は VECTOR と同じく未接続。
        ("SELECT id FROM t ORDER BY h <-> '[1,2]' LIMIT 1", "0A000"),
        ("SELECT id FROM t ORDER BY h <+> '[1,2]' LIMIT 1", "0A000"),
        // 次元不一致は 22000。
        ("SELECT id FROM t ORDER BY h <=> '[1,2,3]' LIMIT 1", "22000"),
        ("SELECT l2_distance(h, '[1,2,3]') FROM t", "22000"),
        ("SELECT id FROM t WHERE h <-> '[1,2,3]' < 1", "22000"),
        // 型の組み合わせ。
        ("SELECT l2_distance(h, n) FROM t", "42883"),
        ("SELECT l2_distance(h, '[1,2]'::halfvec, 1) FROM t", "42883"),
        ("SELECT l2_distance('[1,2]', '[1,2]') FROM t", "0A000"),
        // 裸の HALFVEC を式で使う形。
        ("SELECT vector_norm(h) FROM t", "22000"),
        ("SELECT h + h FROM t", "22000"),
        ("SELECT id FROM t ORDER BY h", "22000"),
        ("SELECT id FROM t WHERE h = '[1,2]'", "22000"),
        ("SELECT h::text FROM t", "22000"),
        // CAST の対象外。
        ("SELECT h::halfvec FROM t", "0A000"),
        ("SELECT n::halfvec FROM t", "0A000"),
        ("SELECT '[1,2]'::halfvec[]", "0A000"),
        ("SELECT '[1,2]'::vector", "42601"),
        // 索引・hybrid・USING PLAN。
        ("CREATE INDEX i ON t USING hnsw (h)", "0A000"),
        ("SELECT id FROM t USING PLAN('x') LIMIT 1", "0A000"),
        (
            "SELECT id FROM t ORDER BY hybrid_rrf(h, '[1,2]', n, 'b') LIMIT 1",
            "0A000",
        ),
        // 表に HALFVEC が無い VECTOR 表の従来動作は不変（`<->` は未接続のまま）。
        ("SELECT id FROM r ORDER BY v <-> '[1,2]' LIMIT 1", "0A000"),
    ];
    for (sql, expected) in cases {
        let err = exec_as(&core, "alice", sql).expect_err(&format!("{sql} must fail"));
        assert_eq!(err.wire_code(), *expected, "{sql}: {err:?}");
        let msg = format!("{err:?}");
        assert!(!msg.contains("[1,2,3]"), "{sql}: error leaked a literal");
    }
    // CHECK・DEFAULT の中には置けない。
    assert_eq!(
        code(
            &core,
            "CREATE TABLE c (id BIGINT PRIMARY KEY, h HALFVEC(2), CHECK (l2_distance(h, '[1,2]') < 1))"
        ),
        "0A000"
    );
    // 副作用なし。
    assert_eq!(ids(&core, "SELECT id FROM t"), vec![1]);
}

/// (7) テナント境界。他テナントの行は投影・WHERE・kNN・関数形 ORDER BY・集計に現れない。
#[test]
fn tenant_boundary_holds_for_distances() {
    let (_g, core) = open("hvd-tenant");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(2))",
    );
    ok(
        &core,
        "INSERT INTO t (id, h) VALUES (1, '[5,5]'), (2, '[9,9]')",
    );
    // bob（別テナント）は同じ表に距離 0 の行を入れる。
    exec_as(&core, "bob", "INSERT INTO t (id, h) VALUES (100, '[1,1]')").expect("bob insert");
    let alice = |sql: &str| -> Vec<i64> {
        query_cells(&core, "alice", sql)
            .iter()
            .map(|r| cell_id(&r[0]))
            .collect()
    };
    assert_eq!(
        alice("SELECT id FROM t ORDER BY h <=> '[1,1]' LIMIT 10"),
        vec![1, 2]
    );
    assert_eq!(
        alice("SELECT id FROM t ORDER BY l2_distance(h, '[1,1]') LIMIT 10"),
        vec![1, 2]
    );
    assert_eq!(
        alice("SELECT id FROM t WHERE h <-> '[1,1]' < 1"),
        Vec::<i64>::new()
    );
    let min = query_cells(&core, "alice", "SELECT min(l2_distance(h, '[1,1]')) FROM t");
    assert!(matches!(min[0][0], Cell::Float(d) if d > 1.0), "{min:?}");
    let bob = query_cells(&core, "bob", "SELECT id, l2_distance(h, '[1,1]') FROM t");
    assert_eq!(bob.len(), 1);
    assert_eq!(cell_id(&bob[0][0]), 100);
}

/// (8) Prepared の Describe と Execute の結果列が一致する（HALFVEC kNN も scan の列メタを返す）。
#[test]
fn describe_matches_execute() {
    let (_g, core) = open("hvd-describe");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(2))",
    );
    ok(&core, "INSERT INTO t (id, h) VALUES (1, '[1,2]')");
    for sql in [
        "SELECT id FROM t ORDER BY h <=> '[1,2]' LIMIT 3",
        "SELECT id, h FROM t ORDER BY h <#> '[1,2]' LIMIT 3",
        "SELECT id, l2_distance(h, '[1,2]') FROM t",
        "SELECT '[0.1]'::halfvec",
    ] {
        let session = SessionState::default();
        let prepared = core.parse_sql_prepared(sql).expect("prepare");
        let described = core
            .describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("{sql} describe: {e:?}"))
            .expect("columns");
        let SqlOutcome::Query(q) = exec_as(&core, "alice", sql).expect("execute") else {
            panic!("query expected");
        };
        assert_eq!(described.len(), q.columns.len(), "{sql}");
        for (d, e) in described.iter().zip(q.columns.iter()) {
            assert_eq!(d, e, "{sql}");
        }
    }
}
