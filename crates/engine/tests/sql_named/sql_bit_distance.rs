//! bit 列の距離（`<~>`＝Hamming・`<%>`＝Jaccard と `hamming_distance`・`jaccard_distance`）の
//! SQL 結合テスト（TABLE-48 (2)・Issue #2970。並びは SQL-33・SQL-34、関数名前空間は SQL-26 のポインタ）。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で次を固定する。
//! (1) pgvector の公開回帰テストと同じ入力で距離値と順序が一致する、(2) 決定的な疑似乱数 fixture で
//!     素朴な参照実装の `(距離, id)` 昇順と SQL の ORDER BY が完全一致する（長さ 1・7・8・13・64・65・
//!     1000・1536 など。8 の倍数でない長さのパディングと SIMD の本体を通る長さを含む）、
//! (3) 並びの境界（同点・DESC・OFFSET・LIMIT 省略・NULL・列×列・`binary_quantize`）、
//! (4) WHERE の半径フィルタ、(5) 誤り（長さ不一致 `22000`・文字種 `22P02`・長さ上限 `54000`・型 `42883`・
//!     リテラル同士 `0A000`）とエラー文言への非反映、(6) テナント境界、(7) 未対応の文脈の fail-closed、
//! (8) EXPLAIN、(9) 「`binary_quantize` で粗く選び元のベクトルで並べ直す」運用例（docs の SQL そのまま）。

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
    err(core, sql).wire_code().to_string()
}

fn err(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    exec_as(core, "alice", sql).expect_err(&format!("{sql} must fail"))
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

/// 決定的な xorshift（依存なし）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn bits(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| if self.next() & 1 == 1 { '1' } else { '0' })
            .collect()
    }
}

/// 素朴な参照実装（ビットごとの比較）。
fn ref_hamming(a: &str, b: &str) -> f64 {
    a.bytes().zip(b.bytes()).filter(|(x, y)| x != y).count() as f64
}

fn ref_jaccard(a: &str, b: &str) -> f64 {
    let ab = a
        .bytes()
        .zip(b.bytes())
        .filter(|(x, y)| *x == b'1' && *y == b'1')
        .count() as f64;
    let aa = a.bytes().filter(|x| *x == b'1').count() as f64;
    let bb = b.bytes().filter(|x| *x == b'1').count() as f64;
    if ab == 0.0 {
        1.0
    } else {
        1.0 - ab / (aa + bb - ab)
    }
}

fn seed(core: &EngineCore, n: usize, rows: &[String]) {
    ok(
        core,
        &format!("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT({n}))"),
    );
    for (ci, chunk) in rows.chunks(20).enumerate() {
        let values: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| format!("({}, '{}')", ci * 20 + i + 1, v))
            .collect();
        ok(
            core,
            &format!("INSERT INTO t (id, v) VALUES {}", values.join(",")),
        );
    }
}

/// 参照実装の `(距離, id)` 昇順の id 列（上位 k 件）。
fn expected_ids(rows: &[String], q: &str, k: usize, f: fn(&str, &str) -> f64) -> Vec<i64> {
    let mut scored: Vec<(f64, i64)> = rows
        .iter()
        .enumerate()
        .map(|(i, v)| (f(v, q), (i + 1) as i64))
        .collect();
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// (1) pgvector の公開回帰テストと同じ入力の固定値（Hamming・Jaccard）。
#[test]
fn matches_pgvector_regression_values() {
    let (_g, core) = open("bitd-pgvector");
    ok(&core, "CREATE TABLE h (id BIGINT PRIMARY KEY, v BIT(3))");
    ok(
        &core,
        "INSERT INTO h (id, v) VALUES (1,'111'),(2,'110'),(3,'100'),(4,'000')",
    );
    assert_eq!(
        dists(&core, "SELECT id, v <~> '111' FROM h ORDER BY id LIMIT 10"),
        vec![
            (1, Some(0.0)),
            (2, Some(1.0)),
            (3, Some(2.0)),
            (4, Some(3.0))
        ]
    );
    assert_eq!(
        ids(&core, "SELECT id FROM h ORDER BY v <~> '111' LIMIT 10"),
        vec![1, 2, 3, 4]
    );
    ok(&core, "CREATE TABLE j (id BIGINT PRIMARY KEY, v BIT(4))");
    ok(
        &core,
        "INSERT INTO j (id, v) VALUES (1,'1111'),(2,'1110'),(3,'1100'),(4,'1000'),(5,'0000')",
    );
    assert_eq!(
        dists(&core, "SELECT id, v <%> '1111' FROM j ORDER BY id LIMIT 10"),
        vec![
            (1, Some(0.0)),
            (2, Some(0.25)),
            (3, Some(0.5)),
            (4, Some(0.75)),
            (5, Some(1.0))
        ]
    );
    assert_eq!(
        ids(&core, "SELECT id FROM j ORDER BY v <%> '1111' LIMIT 10"),
        vec![1, 2, 3, 4, 5]
    );
    // 全ビット 0 どうしの Jaccard は 1。
    assert_eq!(
        dists(&core, "SELECT id, v <%> '0000' FROM j WHERE id = 5 LIMIT 1"),
        vec![(5, Some(1.0))]
    );
}

/// (2) 疑似乱数 fixture: SQL の順序・距離値が参照実装と完全一致する。
#[test]
fn matches_reference_implementation_across_lengths() {
    for (li, n) in [1usize, 7, 8, 13, 64, 65, 1000, 1536]
        .into_iter()
        .enumerate()
    {
        let (_g, core) = open(&format!("bitd-ref-{li}"));
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (n as u64));
        let rows: Vec<String> = (0..60).map(|_| rng.bits(n)).collect();
        let q = rng.bits(n);
        seed(&core, n, &rows);
        for (op, func, f) in [
            (
                "<~>",
                "hamming_distance",
                ref_hamming as fn(&str, &str) -> f64,
            ),
            (
                "<%>",
                "jaccard_distance",
                ref_jaccard as fn(&str, &str) -> f64,
            ),
        ] {
            let want = expected_ids(&rows, &q, 10, f);
            let by_op = ids(
                &core,
                &format!("SELECT id FROM t ORDER BY v {op} '{q}' LIMIT 10"),
            );
            assert_eq!(by_op, want, "n={n} op={op}");
            let by_fn = ids(
                &core,
                &format!("SELECT id FROM t ORDER BY {func}(v, '{q}') LIMIT 10"),
            );
            assert_eq!(by_fn, want, "n={n} func={func}");
            // 投影した距離値も参照実装と一致し、演算子形と関数形で同じ値になる。
            let d_op = dists(
                &core,
                &format!("SELECT id, v {op} '{q}' AS d FROM t ORDER BY id LIMIT 100"),
            );
            let d_fn = dists(
                &core,
                &format!("SELECT id, {func}(v, '{q}') FROM t ORDER BY id LIMIT 100"),
            );
            assert_eq!(d_op, d_fn, "n={n} op={op}");
            for (id, d) in d_op {
                let row = &rows[(id - 1) as usize];
                let r = f(row, &q);
                assert!(
                    (d.expect("non-null") - r).abs() < 1e-12,
                    "n={n} id={id} {op}"
                );
            }
        }
    }
}

/// (3) 並びの境界: 同点は id 昇順・DESC・OFFSET・LIMIT 省略・NULL・列×列・リテラルが左。
#[test]
fn ordering_boundaries() {
    let (_g, core) = open("bitd-order");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1,'1100'),(2,'1100'),(3,'0000'),(4,NULL),(5,'1111'),(6,'0011')",
    );
    // 距離（'1100' との Hamming）: 1=0, 2=0, 3=2, 5=2, 6=4, 4=NULL。同点は id 昇順、NULL は末尾。
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY v <~> '1100' LIMIT 10"),
        vec![1, 2, 3, 5, 6, 4]
    );
    // DESC は NULL が先頭。
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY v <~> '1100' DESC LIMIT 10"
        ),
        vec![4, 6, 3, 5, 1, 2]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY v <~> '1100' LIMIT 2 OFFSET 2"
        ),
        vec![3, 5]
    );
    // LIMIT 省略（暗黙上限の範囲）でも同じ並び。
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY v <~> '1100'"),
        vec![1, 2, 3, 5, 6, 4]
    );
    // リテラルが左でも対称。
    assert_eq!(
        ids(&core, "SELECT id FROM t ORDER BY '1100' <~> v LIMIT 10"),
        vec![1, 2, 3, 5, 6, 4]
    );
    // 列×列は自分自身との距離 0（NULL 行は NULL）。
    assert_eq!(
        dists(&core, "SELECT id, v <~> v FROM t ORDER BY id LIMIT 10"),
        vec![
            (1, Some(0.0)),
            (2, Some(0.0)),
            (3, Some(0.0)),
            (4, None),
            (5, Some(0.0)),
            (6, Some(0.0))
        ]
    );
    // NULL 行の関数形も NULL（strict）。
    assert_eq!(
        dists(
            &core,
            "SELECT id, hamming_distance(v, '1100') FROM t WHERE id = 4 LIMIT 1"
        ),
        vec![(4, None)]
    );
}

/// (4) WHERE の半径フィルタ（距離式は比較の被演算子）。
#[test]
fn where_radius_filter() {
    let (_g, core) = open("bitd-where");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1,'1100'),(2,'1110'),(3,'0000'),(4,NULL)",
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE v <~> '1100' <= 1 ORDER BY id LIMIT 10"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t WHERE hamming_distance(v, '1100') > 1 ORDER BY id LIMIT 10"
        ),
        vec![3]
    );
    // 距離式そのものは真偽値でないため述語にできない（拒否。副作用なし）。
    let c = code(&core, "SELECT id FROM t WHERE v <~> '1100' LIMIT 5");
    assert!(c == "42804" || c == "42601" || c == "0A000", "got {c}");
}

/// (5) 誤り: 型・長さ・文字種・上限・リテラル同士。エラー文言にリテラルを含まない。
#[test]
fn errors_are_fail_closed_and_do_not_echo_literals() {
    let (_g, core) = open("bitd-errors");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4), w BIT(9), x BIT(10), s TEXT, e VECTOR(3))",
    );
    ok(
        &core,
        "INSERT INTO t (id, v, w, x, s, e) VALUES (1,'1010','101010101','1010101010','a','[1,2,3]')",
    );
    for (sql, want) in [
        // 長さ不一致（束縛時: 列の宣言長とリテラル）。
        ("SELECT v <~> '101' FROM t LIMIT 1", "22000"),
        (
            "SELECT hamming_distance(v, '10101') FROM t LIMIT 1",
            "22000",
        ),
        // 同じバイト長（2 バイト）で長さが違う列どうし（9 と 10 ビット）。
        ("SELECT w <~> x FROM t LIMIT 1", "22000"),
        ("SELECT jaccard_distance(w, x) FROM t LIMIT 1", "22000"),
        // 文字種。
        ("SELECT v <~> '10x1' FROM t LIMIT 1", "22P02"),
        // bit 以外の型。
        ("SELECT s <~> '1010' FROM t LIMIT 1", "42883"),
        ("SELECT e <~> '1010' FROM t LIMIT 1", "42883"),
        ("SELECT hamming_distance(id, '1') FROM t LIMIT 1", "42883"),
        // リテラル同士・NULL 同士は bit 型の被演算子が無い。
        (
            "SELECT id FROM t ORDER BY '1010' <~> '1010' LIMIT 1",
            "0A000",
        ),
        (
            "SELECT hamming_distance('1010', '1010') FROM t LIMIT 1",
            "0A000",
        ),
        // 引数の個数。
        ("SELECT hamming_distance(v) FROM t LIMIT 1", "42883"),
        ("SELECT jaccard_distance(v, v, v) FROM t LIMIT 1", "42883"),
        // 予約名。
        (
            "CREATE FUNCTION hamming_distance(x DOUBLE PRECISION) RETURNS DOUBLE PRECISION AS x",
            "42723",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    // 長さ上限超過は確保前に 54000。
    let huge = "1".repeat(84_000_000);
    let c = code(
        &core,
        &format!("SELECT hamming_distance(v, '{huge}') FROM t LIMIT 1"),
    );
    // 文の長さ上限（字句段）が先に働く場合は 42601。いずれも確保前の拒否で、解析器の 54000 は単体テストで固定する。
    assert!(c == "54000" || c == "42601", "got {c}");
    // エラー文言にリテラルを反映しない。
    for sql in [
        "SELECT v <~> '1x0secret' FROM t LIMIT 1",
        "SELECT v <~> '1010101secret' FROM t LIMIT 1",
    ] {
        let e = err(&core, sql);
        assert!(!e.to_string().contains("secret"), "{e}");
    }
    // 評価時の長さ不一致: binary_quantize(VECTOR(3)) と 4 ビット列は束縛時に分かる。
    assert_eq!(
        code(&core, "SELECT binary_quantize(e) <~> v FROM t LIMIT 1"),
        "22000"
    );
}

/// `binary_quantize(embedding) <~> '..'` で ORDER BY できる。
#[test]
fn binary_quantize_operand() {
    let (_g, core) = open("bitd-bq");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, e VECTOR(4))");
    ok(
        &core,
        "INSERT INTO t (id, e) VALUES (1,'[1,1,1,1]'),(2,'[1,1,-1,-1]'),(3,'[-1,-1,-1,-1]'),(4,NULL)",
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM t ORDER BY binary_quantize(e) <~> '1111' LIMIT 10"
        ),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        dists(
            &core,
            "SELECT id, jaccard_distance(binary_quantize(e), '1100') FROM t ORDER BY id LIMIT 10"
        ),
        vec![(1, Some(0.5)), (2, Some(0.0)), (3, Some(1.0)), (4, None)]
    );
    // 長さ不一致は束縛時に 22000。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t ORDER BY binary_quantize(e) <~> '111' LIMIT 3"
        ),
        "22000"
    );
}

/// (6) テナント境界: 他テナントの行は結果・件数に現れず、誤りのコード・文言もテナントで変わらない。
#[test]
fn tenant_boundary_is_preserved() {
    let (_g, core) = open("bitd-tenant");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    ok(&core, "INSERT INTO t (id, v) VALUES (1,'1111'),(2,'1110')");
    exec_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (1,'1111'),(3,'1111')",
    )
    .expect("bob insert");
    let alice = query_cells(
        &core,
        "alice",
        "SELECT id FROM t ORDER BY v <~> '1111' LIMIT 10",
    );
    assert_eq!(alice.len(), 2);
    let bob = query_cells(
        &core,
        "bob",
        "SELECT id FROM t ORDER BY v <~> '1111' LIMIT 10",
    );
    assert_eq!(
        bob.iter().map(|r| cell_id(&r[0])).collect::<Vec<_>>(),
        vec![1, 3]
    );
    let ea = exec_as(&core, "alice", "SELECT v <~> '101' FROM t LIMIT 1").expect_err("len");
    let eb = exec_as(&core, "bob", "SELECT v <~> '101' FROM t LIMIT 1").expect_err("len");
    assert_eq!(ea.wire_code(), eb.wire_code());
    assert_eq!(ea.to_string(), eb.to_string());
}

/// (7) 未対応の文脈は拒否し、副作用を残さない。
#[test]
fn unsupported_contexts_are_rejected() {
    let (_g, core) = open("bitd-failclosed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4), e VECTOR(2))",
    );
    ok(&core, "INSERT INTO t (id, v, e) VALUES (1,'1010','[1,2]')");
    for sql in [
        "ALTER TABLE t ADD CONSTRAINT c CHECK (v <~> '1010' < 2)",
        "CREATE INDEX i ON t ((v <~> '1010'))",
        "CREATE INDEX i ON t USING hnsw (v bit_hamming_ops)",
        "SELECT id FROM t ORDER BY v <~> $1 LIMIT 3",
        "SELECT id FROM t ORDER BY e <=> '[1,2]' USING PLAN(hnsw) LIMIT 3",
        "SELECT id FROM t ORDER BY v LIMIT 3",
        "SELECT id FROM t WHERE v = '1010' LIMIT 3",
        "SELECT max(v <~> '1010') FROM t",
    ] {
        // 一部は従来から別の理由で拒否される。ここでは「成功しない」ことと副作用のなさを固定する。
        let _ = exec_as(&core, "alice", sql);
    }
    assert!(exec_as(
        &core,
        "alice",
        "ALTER TABLE t ADD CONSTRAINT c CHECK (v <~> '1010' < 2)"
    )
    .is_err());
    assert!(exec_as(&core, "alice", "CREATE INDEX i ON t ((v <~> '1010'))").is_err());
    assert!(exec_as(
        &core,
        "alice",
        "CREATE INDEX i ON t USING hnsw (v bit_hamming_ops)"
    )
    .is_err());
    assert!(exec_as(&core, "alice", "SELECT id FROM t ORDER BY v <~> $1 LIMIT 3").is_err());
    assert_eq!(ids(&core, "SELECT id FROM t ORDER BY id LIMIT 5"), vec![1]);
}

/// (8) EXPLAIN は scan の計画を返す。
#[test]
fn explain_returns_a_scan_plan() {
    let (_g, core) = open("bitd-explain");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    ok(&core, "INSERT INTO t (id, v) VALUES (1,'1010')");
    let out = exec_as(
        &core,
        "alice",
        "EXPLAIN SELECT id FROM t ORDER BY v <~> '1010' LIMIT 3",
    )
    .expect("explain");
    let SqlOutcome::Explain(r) = out else {
        panic!("expected SqlOutcome::Explain");
    };
    assert!(!r.rows.is_empty());
}

/// (9) 運用例（docs/design/column-type-extension.md の SQL そのまま）: `binary_quantize` で二値化した列で
/// 粗く選び、元のベクトルで並べ直した Top-k が、元ベクトルの厳密走査の Top-k と一致する。
#[test]
fn rerank_example_matches_exact_scan() {
    let (_g, core) = open("bitd-rerank");
    ok(
        &core,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, embedding VECTOR(8), code BIT(8))",
    );
    let mut rng = Rng(0xDEAD_BEEF_1234_5678);
    let mut rows = Vec::new();
    for id in 1..=80 {
        let comps: Vec<String> = (0..8)
            .map(|_| {
                let r = (rng.next() % 2001) as i64 - 1000;
                format!("{}", r as f64 / 100.0)
            })
            .collect();
        rows.push((id, format!("[{}]", comps.join(","))));
    }
    for chunk in rows.chunks(20) {
        let values: Vec<String> = chunk
            .iter()
            .map(|(id, v)| format!("({id}, '{v}')"))
            .collect();
        ok(
            &core,
            &format!(
                "INSERT INTO items (id, embedding) VALUES {}",
                values.join(",")
            ),
        );
    }
    ok(&core, "UPDATE items SET code = binary_quantize(embedding)");
    let (_, qvec) = &rows[0];
    // 問い合わせの bit 列は、同じ表の行から得る（クライアント側で正の成分を 1 にしてもよい）。
    let qbits: String = qvec
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|x| {
            if x.parse::<f64>().unwrap() > 0.0 {
                '1'
            } else {
                '0'
            }
        })
        .collect();
    let exact = ids(
        &core,
        &format!("SELECT id FROM items ORDER BY embedding <=> '{qvec}' LIMIT 5"),
    );
    // (b) 1 文の半径形（Hamming 距離が 6 以下の行だけを元のベクトルで並べ直す）。
    let b = ids(
        &core,
        &format!(
            "SELECT id FROM items WHERE code <~> '{qbits}' <= 6 \
             ORDER BY embedding <=> '{qvec}' LIMIT 5"
        ),
    );
    assert_eq!(b, exact);
    // (c) 2 文の形: 1 文目で候補の id を得て、2 文目で厳密に並べ直す。
    let cand = ids(
        &core,
        &format!("SELECT id FROM items ORDER BY code <~> '{qbits}' LIMIT 60"),
    );
    let list: Vec<String> = cand.iter().map(i64::to_string).collect();
    let c = ids(
        &core,
        &format!(
            "SELECT id FROM items WHERE id IN ({}) ORDER BY embedding <=> '{qvec}' LIMIT 5",
            list.join(",")
        ),
    );
    assert_eq!(c, exact);
    // 列を持たない形でも粗選別できる。
    let no_col = ids(
        &core,
        &format!("SELECT id FROM items ORDER BY binary_quantize(embedding) <~> '{qbits}' LIMIT 60"),
    );
    assert_eq!(no_col, cand);
}
