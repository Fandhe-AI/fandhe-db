//! `WHERE` 内の距離比較（半径検索）の評価の結合テスト（SQL-74 (1)(2)(3)(5)・Issue #2036）。
//!
//! 境界（等号・負の内積）・NaN の順序・NULL・論理演算との組み合わせ・テスト内の参照実装との
//! 照合（brute-force 一致）・`LIMIT` なし・テナント境界・DML・`HINT ORDER(DISTANCE, ...)` の
//! 取りこぼしが起きないことを確かめる。HNSW を opt-in した経路は `core_misc/sql74_radius_hnsw.rs` が担当する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str, ddl: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, ddl)
        .expect("create table");
    (core, session, CleanupGuard(path))
}

fn insert(core: &EngineCore, session: &mut SessionState, id: usize, vec: &str, n: i64) {
    core.execute_sql_in_session(
        &ctx(),
        session,
        &format!(
            "INSERT INTO d (id, embedding, n) VALUES ({id}, {vec}, {n}) \
             USING OPERATION_ID 'r74-{id}'"
        ),
    )
    .expect("insert");
}

/// 投影した 1 列目（`id`）の値を取り出す（`ResultRow::id` は内部の行 id で、投影の値とは別）。
fn first_id(row: &engine::sql::exec::ResultRow) -> u64 {
    match row.cells.first() {
        Some(engine::sql::exec::Cell::SignedInteger(v)) => u64::try_from(*v).expect("non-negative"),
        other => panic!("unexpected cell: {other:?}"),
    }
}

/// 結果の id を昇順で返す。
fn ids(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<u64> {
    match core
        .execute_sql_in_session(&ctx(), session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
    {
        SqlOutcome::Query(r) => {
            let mut v: Vec<u64> = r.rows.iter().map(first_id).collect();
            v.sort_unstable();
            v
        }
        _ => panic!("expected Query outcome"),
    }
}

/// 結果の id を返却順のまま返す。
fn ids_ordered(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<u64> {
    match core
        .execute_sql_in_session(&ctx(), session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
    {
        SqlOutcome::Query(r) => r.rows.iter().map(first_id).collect(),
        _ => panic!("expected Query outcome"),
    }
}

/// 境界テスト用の小さな表（値はすべて f32 で厳密）。
fn boundary_table(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let (core, mut session, g) = setup(
        label,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, embedding VECTOR(3), n INTEGER)",
    );
    for (i, v) in [
        "'[0,0,0]'",
        "'[3,4,0]'",
        "'[2,0,0]'",
        "'[1,0,0]'",
        "'[0,1,0]'",
    ]
    .iter()
    .enumerate()
    {
        insert(&core, &mut session, i, v, i as i64);
    }
    (core, session, g)
}

fn q(core: &EngineCore, s: &mut SessionState, where_clause: &str) -> Vec<u64> {
    ids(core, s, &format!("SELECT id FROM d WHERE {where_clause}"))
}

/// 距離がしきい値ちょうどの行は `<=`／`=` に含まれ、`<` には含まれない（L2・L1）。
#[test]
fn equality_boundary_l2_and_l1() {
    let (core, mut s, _g) = boundary_table("r74-eq");
    let l2 = "embedding <-> '[3,4,0]'";
    assert_eq!(q(&core, &mut s, &format!("{l2} <= 5")), vec![0, 1, 2, 3, 4]);
    assert_eq!(q(&core, &mut s, &format!("{l2} < 5")), vec![1, 2, 3, 4]);
    assert_eq!(q(&core, &mut s, &format!("{l2} = 5")), vec![0]);
    assert_eq!(q(&core, &mut s, &format!("{l2} >= 5")), vec![0]);
    assert_eq!(q(&core, &mut s, &format!("{l2} > 5")), Vec::<u64>::new());
    let l1 = "embedding <+> '[3,4,0]'";
    assert_eq!(q(&core, &mut s, &format!("{l1} <= 6")), vec![1, 2, 3, 4]);
    assert_eq!(q(&core, &mut s, &format!("{l1} < 6")), vec![1, 2]);
}

/// 負の内積の境界（値は 0, -3, -2, -1, 0）。
#[test]
fn negative_inner_product_boundary() {
    let (core, mut s, _g) = boundary_table("r74-nip");
    let d = "embedding <#> '[1,0,0]'";
    assert_eq!(q(&core, &mut s, &format!("{d} < -1")), vec![1, 2]);
    assert_eq!(q(&core, &mut s, &format!("{d} <= -2")), vec![1, 2]);
    assert_eq!(q(&core, &mut s, &format!("{d} < -2")), vec![1]);
    assert_eq!(q(&core, &mut s, &format!("{d} <= -3")), vec![1]);
    assert_eq!(q(&core, &mut s, &format!("{d} < -3")), Vec::<u64>::new());
    assert_eq!(q(&core, &mut s, &format!("{d} = 0")), vec![0, 4]);
}

/// コサイン距離（ゼロノルムの行 0 は NaN。PG の float8 順序で NaN は最大・NaN 同士は等しい）。
#[test]
fn cosine_nan_ordering() {
    let (core, mut s, _g) = boundary_table("r74-nan");
    let d = "embedding <=> '[1,0,0]'";
    // 距離: id0=NaN, id1=0.4, id2=0, id3=0, id4=1
    assert_eq!(q(&core, &mut s, &format!("{d} < 1")), vec![1, 2, 3]);
    assert_eq!(q(&core, &mut s, &format!("{d} = 1")), vec![4]);
    assert_eq!(q(&core, &mut s, &format!("{d} > 1")), vec![0]);
    assert_eq!(q(&core, &mut s, &format!("{d} >= 1")), vec![0, 4]);
    assert_eq!(q(&core, &mut s, &format!("{d} <> 1")), vec![0, 1, 2, 3]);
    assert_eq!(q(&core, &mut s, &format!("{d} != 1")), vec![0, 1, 2, 3]);
    assert_eq!(
        q(&core, &mut s, &format!("{d} BETWEEN 0 AND 2")),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        q(&core, &mut s, &format!("{d} NOT BETWEEN 0 AND 2")),
        vec![0]
    );
    assert_eq!(q(&core, &mut s, &format!("NOT {d} < 1")), vec![0, 4]);
    assert_eq!(q(&core, &mut s, &format!("1 > {d}")), vec![1, 2, 3]);
}

/// AND／OR／NOT／逆向き／括弧／式の右辺との組み合わせ。
#[test]
fn logical_combinations() {
    let (core, mut s, _g) = boundary_table("r74-logic");
    let d = "embedding <-> '[3,4,0]'";
    assert_eq!(q(&core, &mut s, &format!("{d} < 5 AND n >= 3")), vec![3, 4]);
    assert_eq!(
        q(&core, &mut s, &format!("{d} < 4.2 OR n = 0")),
        vec![0, 1, 2]
    );
    assert_eq!(q(&core, &mut s, &format!("({d}) < 4.2")), vec![1, 2]);
    assert_eq!(q(&core, &mut s, &format!("{d} < 4.1 + 0.1")), vec![1, 2]);
    assert_eq!(q(&core, &mut s, &format!("4.2 > {d}")), vec![1, 2]);
    assert_eq!(
        q(&core, &mut s, "'[3,4,0]' <-> embedding < 4.2"),
        vec![1, 2]
    );
    assert_eq!(q(&core, &mut s, &format!("NOT ({d} < 4.2)")), vec![0, 3, 4]);
}

/// DML は参照実装どおりの行だけを書き換える。
#[test]
fn dml_uses_distance_predicate() {
    let (core, mut s, _g) = boundary_table("r74-dml");
    core.execute_sql_in_session(
        &ctx(),
        &mut s,
        "UPDATE d SET n = 90 WHERE embedding <-> '[3,4,0]' < 4.2 USING OPERATION_ID 'u-1'",
    )
    .expect("update");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM d WHERE n = 90"),
        vec![1, 2]
    );
    core.execute_sql_in_session(
        &ctx(),
        &mut s,
        "DELETE FROM d WHERE embedding <-> '[3,4,0]' >= 5 USING OPERATION_ID 'd-1'",
    )
    .expect("delete");
    assert_eq!(ids(&core, &mut s, "SELECT id FROM d"), vec![1, 2, 3, 4]);
}

/// 決定的な格子データ（値は 1/4 刻みで f32 に厳密。累積順序によらず参照実装と一致する）。
fn grid_vec(i: usize) -> [f32; 4] {
    let mut x = (i as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut out = [0.0f32; 4];
    for o in &mut out {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *o = (((x >> 33) % 17) as i64 - 8) as f32 / 4.0;
    }
    out
}

fn ref_distance(kind: &str, a: &[f32; 4], b: &[f32; 4]) -> f64 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    match kind {
        "<->" => f64::from(a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>()).sqrt(),
        "<#>" => -f64::from(dot),
        "<+>" => f64::from(a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>()),
        _ => {
            let na: f32 = a.iter().map(|x| x * x).sum();
            let nb: f32 = b.iter().map(|x| x * x).sum();
            1.0 - (f64::from(dot) / (f64::from(na) * f64::from(nb)).sqrt()).clamp(-1.0, 1.0)
        }
    }
}

fn grid_table(label: &str, rows: usize) -> (EngineCore, SessionState, CleanupGuard) {
    let (core, mut s, g) = setup(
        label,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, embedding VECTOR(4), n INTEGER)",
    );
    for i in 0..rows {
        let v = grid_vec(i);
        insert(
            &core,
            &mut s,
            i,
            &format!("'[{},{},{},{}]'", v[0], v[1], v[2], v[3]),
            (i % 10) as i64,
        );
    }
    (core, s, g)
}

/// 4 種別 x 複数しきい値で、`SELECT id ... WHERE dist < r` がテスト内の参照実装（全行走査）と一致する。
/// しきい値は距離の隣り合う相異なる値の中点に取る（境界の丸めに依存しない）。
#[test]
fn brute_force_matches_reference() {
    let rows = 200;
    let (core, mut s, _g) = grid_table("r74-bf", rows);
    let query = [1.0f32, -0.5, 0.25, 2.0];
    let qlit = "[1,-0.5,0.25,2]";
    for kind in ["<=>", "<->", "<#>", "<+>"] {
        let mut dist: Vec<(u64, f64)> = (0..rows)
            .map(|i| (i as u64, ref_distance(kind, &grid_vec(i), &query)))
            .filter(|(_, d)| !d.is_nan())
            .collect();
        dist.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut distinct: Vec<f64> = dist.iter().map(|x| x.1).collect();
        distinct.dedup();
        for pos in [3usize, distinct.len() / 3, distinct.len() * 2 / 3] {
            let r = (distinct[pos] + distinct[pos + 1]) / 2.0;
            let mut want: Vec<u64> = dist.iter().filter(|x| x.1 < r).map(|x| x.0).collect();
            want.sort_unstable();
            let got = ids(
                &core,
                &mut s,
                &format!("SELECT id FROM d WHERE embedding {kind} '{qlit}' < {r:e}"),
            );
            assert_eq!(got, want, "{kind} r={r}");
            let mut want_ge: Vec<u64> = (0..rows as u64)
                .filter(|i| {
                    let d = ref_distance(kind, &grid_vec(*i as usize), &query);
                    d.is_nan() || d >= r
                })
                .collect();
            want_ge.sort_unstable();
            let got_ge = ids(
                &core,
                &mut s,
                &format!("SELECT id FROM d WHERE embedding {kind} '{qlit}' >= {r:e}"),
            );
            assert_eq!(got_ge, want_ge, "{kind} >= r={r}");
        }
    }
}

/// kNN と併用した `ORDER BY ... LIMIT k [OFFSET m]` は、参照実装の「距離で絞り込み → 内積の降順
/// （同点は id 昇順）で上位」と順序まで一致する。順位付けは `<#>`（負の内積の昇順＝内積の降順。
/// Issue #1991 で接続。`<=>` はコサイン距離に変わったため、内積の参照実装とは `<#>` で照合する）。
#[test]
fn knn_with_distance_filter_matches_reference() {
    let rows = 200;
    let (core, mut s, _g) = grid_table("r74-knn", rows);
    let query = [1.0f32, -0.5, 0.25, 2.0];
    let qlit = "[1,-0.5,0.25,2]";
    let mut l2: Vec<f64> = (0..rows)
        .map(|i| ref_distance("<->", &grid_vec(i), &query))
        .collect();
    l2.sort_by(f64::total_cmp);
    l2.dedup();
    let r = (l2[l2.len() / 2] + l2[l2.len() / 2 + 1]) / 2.0;
    let mut cand: Vec<(u64, f32)> = (0..rows)
        .filter(|i| ref_distance("<->", &grid_vec(*i), &query) < r)
        .map(|i| {
            let v = grid_vec(i);
            (
                i as u64,
                v.iter().zip(&query).map(|(a, b)| a * b).sum::<f32>(),
            )
        })
        .collect();
    cand.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<u64> = cand.iter().map(|x| x.0).collect();
    let sql = |lim: usize, off: usize| {
        format!(
            "SELECT id FROM d WHERE embedding <-> '{qlit}' < {r:e} \
             ORDER BY embedding <#> '{qlit}' LIMIT {lim} OFFSET {off}"
        )
    };
    assert_eq!(ids_ordered(&core, &mut s, &sql(10, 0)), want[..10].to_vec());
    assert_eq!(ids_ordered(&core, &mut s, &sql(7, 5)), want[5..12].to_vec());
}

/// `HINT ORDER(DISTANCE, SCALAR, RLS)` を指定しても、距離述語を含む文は事前フィルタに倒されるため
/// （初回の上位 k 件が述語で間引かれる）取りこぼしが起きず、`LIMIT` 件が埋まる。
#[test]
fn distance_first_hint_still_fills_limit() {
    let rows = 200;
    let (core, mut s, _g) = grid_table("r74-hint", rows);
    let qlit = "[1,-0.5,0.25,2]";
    // 近い行の多くを下限で除外する（距離の小さい側が落ちるので、初回の上位 k は大半が不通過）。
    let base = format!(
        "SELECT id FROM d WHERE embedding <-> '{qlit}' > 3.0 AND embedding <-> '{qlit}' < 6.0 \
         ORDER BY embedding <=> '{qlit}' LIMIT 12"
    );
    let pre = ids_ordered(&core, &mut s, &base);
    assert_eq!(pre.len(), 12);
    let post = ids_ordered(
        &core,
        &mut s,
        &format!("{base} HINT ORDER(DISTANCE, SCALAR, RLS)"),
    );
    assert_eq!(post, pre);
}

/// `LIMIT` なし（ORDER BY なし）は全可視行を厳密に評価する。ORDER BY があって `LIMIT` が無い形も受理し、厳密全走査で距離昇順に返す。
#[test]
fn no_limit_is_exact_and_order_by_without_limit_is_accepted() {
    let rows = 200;
    let (core, mut s, _g) = grid_table("r74-nolimit", rows);
    let query = [1.0f32, -0.5, 0.25, 2.0];
    let want: Vec<u64> = (0..rows as u64)
        .filter(|i| ref_distance("<+>", &grid_vec(*i as usize), &query) <= 6.0)
        .collect();
    let got = ids(
        &core,
        &mut s,
        "SELECT id FROM d WHERE embedding <+> '[1,-0.5,0.25,2]' <= 6",
    );
    assert_eq!(got, want);
    // LIMIT 省略の ORDER BY は受理され、半径内の全行が距離昇順で返る（厳密全走査）。
    // 順位付けは `<#>`（負の内積の昇順＝内積の降順。Issue #1991 で接続）。
    let ordered = ids_ordered(
        &core,
        &mut s,
        "SELECT id FROM d WHERE embedding <+> '[1,-0.5,0.25,2]' <= 6 \
         ORDER BY embedding <#> '[1,-0.5,0.25,2]'",
    );
    let mut sorted_ids = ordered.clone();
    sorted_ids.sort_unstable();
    assert_eq!(sorted_ids, want);
    // 順序は内積の降順（同点は id 昇順）。
    let dots: Vec<f32> = ordered
        .iter()
        .map(|i| {
            grid_vec(*i as usize)
                .iter()
                .zip(&query)
                .map(|(a, b)| a * b)
                .sum::<f32>()
        })
        .collect();
    assert!(dots.windows(2).all(|w| w[0] >= w[1]), "{dots:?}");
}

/// 他テナントの Private 行がクエリの直近にあっても、自テナントの結果にも件数にも現れない。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, mut s, _g) = boundary_table("r74-tenant");
    let other =
        PolicyContext::with_visibilities("other", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut os = SessionState::default();
    core.execute_sql_in_session(
        &other,
        &mut os,
        "INSERT INTO d (id, embedding, n) VALUES (100, '[3,4,0]', 1) USING OPERATION_ID 'o-100'",
    )
    .expect("other tenant insert must succeed");
    let sql = "SELECT id FROM d WHERE embedding <-> '[3,4,0]' < 0.5";
    // 他テナントのコンテキストからは挿入行が距離述語に一致する（前提の成立確認）。
    match core
        .execute_sql_in_session(&other, &mut os, sql)
        .expect("other tenant select")
    {
        SqlOutcome::Query(r) => {
            let got: Vec<u64> = r.rows.iter().map(first_id).collect();
            assert!(got.contains(&100), "other tenant must match its own row");
        }
        _ => panic!("expected Query outcome"),
    }
    assert_eq!(ids(&core, &mut s, sql), vec![1]);
}

/// エラー文言にベクトルリテラルの値を含めない。
#[test]
fn error_detail_does_not_echo_literal() {
    let (core, mut s, _g) = boundary_table("r74-leak");
    let err = core
        .execute_sql_in_session(
            &ctx(),
            &mut s,
            "SELECT id FROM d WHERE embedding <-> '[7.654321,0]' < 1",
        )
        .expect_err("dim mismatch");
    assert!(!format!("{err}").contains("7.654321"));
}

/// 件数だけを返す集計など embedding を投影しない文脈でも、距離述語は VECTOR 列をデコードして
/// 評価される（空の embedding で NULL 扱いになって行が黙って消えない）。
#[test]
fn contexts_without_embedding_projection_still_evaluate() {
    let (core, mut s, _g) = boundary_table("r74-ctx");
    let cell = |core: &EngineCore, s: &mut SessionState, sql: &str| -> i64 {
        match core
            .execute_sql_in_session(&ctx(), s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
        {
            SqlOutcome::Query(r) => match r.rows.first().and_then(|r| r.cells.first()) {
                Some(engine::sql::exec::Cell::SignedInteger(v)) => *v,
                Some(engine::sql::exec::Cell::Integer(v)) => i64::try_from(*v).expect("fits"),
                other => panic!("{sql}: unexpected {other:?}"),
            },
            _ => panic!("expected Query outcome"),
        }
    };
    let w = "embedding <-> '[3,4,0]' < 4.2";
    assert_eq!(
        cell(&core, &mut s, &format!("SELECT count(*) FROM d WHERE {w}")),
        2
    );
    assert_eq!(
        cell(&core, &mut s, &format!("SELECT max(n) FROM d WHERE {w}")),
        2
    );
    assert_eq!(
        cell(
            &core,
            &mut s,
            &format!("SELECT count(*) FROM d WHERE {w} OR n = 4")
        ),
        3
    );
}
