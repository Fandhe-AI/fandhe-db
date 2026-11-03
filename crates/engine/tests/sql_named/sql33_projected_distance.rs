//! 投影位置の距離列 `SELECT id, embedding <=> q AS d ... ORDER BY embedding <=> q` の受け入れ
//! テスト（Issue #2007。ビヘイビア: SQL-33 (7)。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 固定する契約:
//! - 距離列は float8（`ColumnType::Double`）。別名なしの列名は `?column?`。
//! - 値は行ごとのコサイン距離（`1 - cos`。ゼロノルム行は NaN）で、結果順と整合する。
//! - 列・演算子種別・クエリベクトルが距離 `ORDER BY` と同一の形だけを受理し、他は fail-closed。
//! - 他テナントの行（と距離値）は結果に入らない。Describe は 4 演算子とも float8 を返す。
//!
//! 非 Cosine の距離 ORDER BY の実行はカーネル接続（#1981・#1991）まで `0A000` のまま。接続時に
//! 本ファイルの該当テストを値検証へ更新する（#1992 の一括更新対象）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 3;

/// (id, embedding)。id 1〜4 は L2 正規化済み、id 5 はゼロノルム。
fn rows() -> Vec<(u64, [f32; DIM])> {
    vec![
        (1, [1.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0]),
        (3, [0.6, 0.8, 0.0]),
        (4, [-1.0, 0.0, 0.0]),
        (5, [0.0, 0.0, 0.0]),
    ]
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

fn seed_tenant(core: &EngineCore, tenant: &str, rows: &[(u64, [f32; DIM])]) {
    let ctx = ctx(tenant);
    let mut session = SessionState::default();
    for (id, v) in rows {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[{},{},{}]', 'b{id}') USING OPERATION_ID 'seed-{tenant}-{id}'",
            v[0], v[1], v[2]
        );
        core.execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
    }
}

fn setup(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    seed_tenant(&core, "tenant-a", &rows());
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        SqlOutcome::Query(result) => result,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn err_code(core: &EngineCore, sql: &str) -> String {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql) {
        Err(e) => e.wire_code().to_string(),
        Ok(_) => panic!("{sql} must fail"),
    }
}

fn ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// 距離列（cells の末尾）を f64 で取り出す。
fn dist(result: &QueryResult, row: usize) -> f64 {
    match result.rows[row].cells.last() {
        Some(Cell::Float(f)) => *f,
        other => panic!("distance cell must be Float, got {other:?}"),
    }
}

/// テスト側の独立した参照実装（f64）。ゼロノルムは NaN。
fn ref_cosine_distance(a: &[f32; DIM], b: &[f32; DIM]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let na: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return f64::NAN;
    }
    1.0 - dot / (na * nb)
}

fn row_vec(id: u64) -> [f32; DIM] {
    rows().into_iter().find(|(i, _)| *i == id).expect("row").1
}

#[test]
fn sql33_projected_distance_matches_reference_and_order() {
    let (core, _g) = setup("sql33-proj-dist");
    let r = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 4",
    );
    assert_eq!(
        r.columns.last(),
        Some(&ColumnMeta::Computed {
            name: "d".to_string(),
            ty: Some(ColumnType::Double)
        })
    );
    assert!(!r.rows.is_empty());
    let q = [1.0, 0.0, 0.0];
    for (i, row) in r.rows.iter().enumerate() {
        let expected = ref_cosine_distance(&row_vec(row.id), &q);
        let got = dist(&r, i);
        if expected.is_nan() {
            assert!(got.is_nan(), "id {}", row.id);
        } else {
            assert!(
                (got - expected).abs() < 1e-6,
                "id {}: {got} vs {expected}",
                row.id
            );
        }
    }
    // 有限値の距離は結果順と整合（昇順）。
    let finite: Vec<f64> = (0..r.rows.len())
        .map(|i| dist(&r, i))
        .filter(|d| d.is_finite())
        .collect();
    assert!(finite.windows(2).all(|w| w[0] <= w[1] + 1e-9), "{finite:?}");
}

#[test]
fn sql33_projected_distance_default_name_and_desc_offset() {
    let (core, _g) = setup("sql33-proj-dist-name");
    let r = run(
        &core,
        "tenant-a",
        "SELECT embedding <=> '[1,0,0]' FROM documents WHERE id < 5 ORDER BY embedding <=> '[1,0,0]' LIMIT 4",
    );
    assert_eq!(
        r.columns,
        vec![ColumnMeta::Computed {
            name: "?column?".to_string(),
            ty: Some(ColumnType::Double)
        }]
    );
    let asc = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents WHERE id < 5 ORDER BY embedding <=> '[1,0,0]' LIMIT 4",
    );
    assert_eq!(ids(&asc), vec![1, 3, 2, 4]);
    let desc = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents WHERE id < 5 ORDER BY embedding <=> '[1,0,0]' DESC LIMIT 4",
    );
    assert_eq!(ids(&desc), vec![4, 2, 3, 1]);
    assert!((dist(&desc, 0) - 2.0).abs() < 1e-6);
    let paged = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents WHERE id < 5 ORDER BY embedding <=> '[1,0,0]' LIMIT 2 OFFSET 1",
    );
    assert_eq!(ids(&paged), vec![3, 2]);
    assert!((dist(&paged, 0) - 0.4).abs() < 1e-6);
    let all = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents WHERE id < 5 ORDER BY embedding <=> '[1,0,0]'",
    );
    assert_eq!(ids(&all), vec![1, 3, 2, 4]);
}

#[test]
fn sql33_projected_distance_zero_norm_row_is_nan() {
    let (core, _g) = setup("sql33-proj-dist-nan");
    let r = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 5",
    );
    let idx = r.rows.iter().position(|row| row.id == 5).expect("id 5");
    assert!(dist(&r, idx).is_nan());
}

#[test]
fn sql33_projected_distance_with_scalar_column_is_stable_across_cache() {
    let (core, _g) = setup("sql33-proj-dist-cache");
    let sql = "SELECT id, body, embedding <=> '[1,0,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3";
    let first = run(&core, "tenant-a", sql);
    let second = run(&core, "tenant-a", sql);
    assert_eq!(ids(&first), ids(&second));
    for i in 0..first.rows.len() {
        assert_eq!(first.rows[i].cells[0], second.rows[i].cells[0]);
        assert_eq!(dist(&first, i).to_bits(), dist(&second, i).to_bits());
    }
    let filtered = run(
        &core,
        "tenant-a",
        "SELECT id, body, embedding <=> '[1,0,0]' AS d FROM documents WHERE body = 'b3' ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
    );
    assert_eq!(ids(&filtered), vec![3]);
    assert!((dist(&filtered, 0) - 0.4).abs() < 1e-6);
}

#[test]
fn sql33_projected_distance_excludes_other_tenant_rows() {
    let (core, _g) = setup("sql33-proj-dist-tenant");
    seed_tenant(&core, "tenant-b", &[(100, [1.0, 0.0, 0.0])]);
    let r = run(
        &core,
        "tenant-a",
        "SELECT id, embedding <=> '[1,0,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 10",
    );
    assert!(!ids(&r).contains(&100));
    assert_eq!(r.rows.len(), 5);
}

#[test]
fn sql33_projected_distance_non_cosine_execution_stays_fail_closed() {
    let (core, _g) = setup("sql33-proj-dist-l2");
    for op in ["<->", "<#>", "<+>"] {
        let sql = format!(
            "SELECT id, embedding {op} '[1,0,0]' AS d FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3"
        );
        assert_eq!(err_code(&core, &sql), "0A000", "{op}");
    }
}

#[test]
fn sql33_projected_distance_rejects_unsupported_shapes() {
    let (core, _g) = setup("sql33-proj-dist-reject");
    let cases: &[(&str, &str)] = &[
        // 演算子種別の不一致
        (
            "SELECT embedding <-> '[1,0,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "42601",
        ),
        // 距離 ORDER BY なし（広域取得）
        (
            "SELECT embedding <=> '[1,0,0]' AS d FROM documents LIMIT 3",
            "42601",
        ),
        // スカラー ORDER BY
        (
            "SELECT embedding <=> '[1,0,0]' AS d FROM documents ORDER BY id LIMIT 3",
            "42601",
        ),
        // 入れ子・逆向き・算術合成
        (
            "SELECT 1 - (embedding <=> '[1,0,0]') AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "42601",
        ),
        (
            "SELECT '[1,0,0]' <=> embedding AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "42601",
        ),
        (
            "SELECT embedding <=> '[1,0,0]' * 2 AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "42601",
        ),
        // クエリベクトルの不一致
        (
            "SELECT embedding <=> '[0,1,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "0A000",
        ),
        // 次元不一致
        (
            "SELECT embedding <=> '[1,0]' AS d FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 3",
            "22000",
        ),
    ];
    for (sql, code) in cases {
        assert_eq!(&err_code(&core, sql), code, "{sql}");
    }
}

#[test]
fn sql33_projected_distance_prepared_describe_and_execute() {
    let (core, _g) = setup("sql33-proj-dist-prepared");
    let session = SessionState::default();
    for op in ["<=>", "<->", "<#>", "<+>"] {
        let sql = format!(
            "SELECT id, embedding {op} $1 AS d FROM documents ORDER BY embedding {op} $1 LIMIT 3"
        );
        let prepared = core.parse_sql_prepared(&sql).expect("parse");
        let cols = core
            .describe_prepared_in_session(&session, &prepared)
            .expect("describe")
            .expect("columns");
        assert_eq!(
            cols.last(),
            Some(&ColumnMeta::Computed {
                name: "d".to_string(),
                ty: Some(ColumnType::Double)
            }),
            "{op}"
        );
        // 実リテラルの Describe も同じ列メタ。
        let literal_sql = format!(
            "SELECT id, embedding {op} '[1,0,0]' AS d FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3"
        );
        let parsed = core.parse_sql(&literal_sql).expect("parse literal");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe literal")
            .expect("columns");
        assert_eq!(described, cols, "{op}");
    }
    // Bind／Execute（<=>）: `$1` を投影と ORDER BY の両方に使う。
    let prepared = core
        .parse_sql_prepared(
            "SELECT id, embedding <=> $1 AS d FROM documents ORDER BY embedding <=> $1 LIMIT 3",
        )
        .expect("parse");
    let parsed = core
        .bind_prepared(&prepared, &[Some(b"[1,0,0]".to_vec())])
        .expect("bind");
    let mut s = SessionState::default();
    match core
        .execute_parsed_in_session(&ctx("tenant-a"), &mut s, &parsed)
        .expect("execute")
    {
        SqlOutcome::Query(r) => {
            assert_eq!(ids(&r)[0], 1);
            assert!(dist(&r, 0).abs() < 1e-6);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

/// PR #2927 指摘の回帰: Prepared Describe の検証省略は `$n` 由来のダミー値の位置に限る。
/// 投影側だけ・ORDER BY 側だけに `$n` がある文でも、もう一方の不正な実リテラルは
/// Describe（Bind 前）で `22000` として検出する（通常の Describe と同じ）。
#[test]
fn sql33_projected_distance_prepared_describe_validates_real_literal_beside_param() {
    let (core, _g) = setup("sql33-proj-dist-prepared-mixed");
    let session = SessionState::default();
    for sql in [
        // 投影のみ `$1`、ORDER BY は不正な実リテラル。
        "SELECT embedding <=> $1 AS d FROM documents ORDER BY embedding <=> 'invalid' LIMIT 3",
        // ORDER BY のみ `$1`、投影は不正な実リテラル。
        "SELECT embedding <=> 'invalid' AS d FROM documents ORDER BY embedding <=> $1 LIMIT 3",
        // 投影内の `$1` 項目と不正な実リテラル項目の混在。
        "SELECT embedding <=> $1 AS a, embedding <=> 'invalid' AS b FROM documents ORDER BY embedding <=> $1 LIMIT 3",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let err = core
            .describe_prepared_in_session(&session, &prepared)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "22000", "{sql}");
    }
}

/// PR #2927 指摘の回帰: ORDER BY が実リテラルのとき、投影内の `$n` 項目があっても
/// 実リテラルの距離項目はクエリベクトル一致を Describe 時点で検査し、通常の Describe と
/// 同じく `0A000` で拒否する（項目ごとの判定）。
#[test]
fn sql33_projected_distance_prepared_describe_checks_literal_item_beside_param_item() {
    let (core, _g) = setup("sql33-proj-dist-prepared-item-mismatch");
    let session = SessionState::default();
    let sql = "SELECT embedding <=> $1 AS a, embedding <=> '[0,1,0]' AS b FROM documents \
               ORDER BY embedding <=> '[1,0,0]' LIMIT 3";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    let err = core
        .describe_prepared_in_session(&session, &prepared)
        .expect_err(sql);
    assert_eq!(err.wire_code(), "0A000", "{sql}");
}
