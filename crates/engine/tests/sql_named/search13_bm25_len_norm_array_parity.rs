//! `score_pass` の `k1 * len_norm` キャッシュを文書長クラス添字の配列にした変更
//! （Issue #3222。SEARCH-13・RLS-8 のポインタ表記）の SQL 経由の結合テスト。
//!
//! SQL からは BM25 スコアのビット列そのものは見えないため、スコアのビット一致は
//! `sparse.rs` の単体テスト側で固定し、本テストは順位・id・NULL・拒否コード・
//! テナント境界を固定する。tenant-a と同じ文書長クラスを共有する行を tenant-b にも
//! 置き、tenant-b の有無で tenant-a の結果（全列の Debug 表現）が変わらないこと、
//! 同一セッションで `k1`／`b` を変えた問い合わせを連続実行しても stale 値が混入せず
//! 同じ問い合わせの再実行が同一結果になることを確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

type Row = (u64, [f32; 2], Option<String>);

fn seed(storage: &Storage, tenant: &str, vis: Visibility, rows: &[Row]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for (id, emb, body) in rows {
        let body = match body {
            Some(b) => Value::Text(b.clone()),
            None => Value::Null,
        };
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            vis,
            &[Value::Vector(emb.to_vec()), body],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{tenant}-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

/// tenant-a は語 `apple` を 0..5 個含み、語数（文書長）が行ごとに異なる。
fn tenant_a_rows() -> Vec<Row> {
    (1..=40u64)
        .map(|id| {
            let apples = (id % 5) as usize;
            let fillers = (id % 7) as usize + 1;
            let mut words = vec!["apple"; apples];
            words.extend(std::iter::repeat_n("zzz", fillers));
            let body = if id % 11 == 0 {
                None
            } else {
                Some(words.join(" "))
            };
            (id, [1.0 - id as f32 * 0.01, id as f32 * 0.01], body)
        })
        .collect()
}

fn setup(name: &str, with_tenant_b: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    seed(&storage, "tenant-a", Visibility::Public, &tenant_a_rows());
    if with_tenant_b {
        // tenant-a と同じ文書長クラスを共有しうる行。クエリ語を大量に含む。
        let rows: Vec<Row> = (101..=130u64)
            .map(|id| {
                let n = (id % 9) as usize + 1;
                (id, [1.0, 0.0], Some(vec!["apple"; n].join(" ")))
            })
            .collect();
        seed(&storage, "tenant-b", Visibility::Private, &rows);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, tail: &str) -> Result<QueryResult, &'static str> {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let sql = format!(
        "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple'{tail}) LIMIT 40"
    );
    core.execute_sql(&ctx, &sql).map_err(|e| e.wire_code())
}

const TAILS: &[&str] = &[
    "",
    ", k1 => 1.5, b => 0.5",
    ", b => 0.0",
    ", b => 1.0",
    ", k1 => 0.0",
    ", k1 => 1e200",
];

#[test]
fn search13_len_norm_array_results_are_independent_of_other_tenants_rows() {
    let (with_b, _g1) = setup("search13-lnarr-with-b", true);
    let (without_b, _g2) = setup("search13-lnarr-without-b", false);
    // 同じコアで params を変えながら 2 周し、キャッシュ済み索引のスクラッチ再利用で
    // stale 値が混入しないこと（再実行が同一）も確認する。
    for _round in 0..2 {
        for tail in TAILS {
            let a = run(&with_b, "tenant-a", tail).expect("with tenant-b");
            let b = run(&without_b, "tenant-a", tail).expect("without tenant-b");
            assert_eq!(format!("{:?}", a.rows), format!("{:?}", b.rows), "{tail}");
            assert!(
                a.rows.iter().all(|r| r.id <= 40),
                "{tail}: foreign id leaked"
            );
            let again = run(&with_b, "tenant-a", tail).expect("again");
            assert_eq!(
                format!("{:?}", a.rows),
                format!("{:?}", again.rows),
                "{tail}"
            );
        }
    }
}

#[test]
fn search13_len_norm_array_overflow_k1_is_still_rejected_as_22000() {
    let (core, _g) = setup("search13-lnarr-overflow", true);
    assert_eq!(run(&core, "tenant-a", ", k1 => 1e201").err(), Some("22000"));
    // 拒否の後でも正常な問い合わせは従来どおり成功する。
    assert!(run(&core, "tenant-a", ", k1 => 1e200").is_ok());
}

#[test]
fn search13_len_norm_array_no_match_and_third_tenant_do_not_leak() {
    let (core, _g) = setup("search13-lnarr-empty", true);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let no_match = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'nomatchterm') LIMIT 40",
        )
        .expect("no-match query");
    assert!(no_match.rows.iter().all(|r| r.id <= 40));
    // 第三テナントには Public 行（tenant-a 由来）だけが見え、tenant-b の Private 行は現れない。
    let other = run(&core, "tenant-z", "").expect("third tenant");
    assert!(other.rows.iter().all(|r| r.id <= 40));
}
