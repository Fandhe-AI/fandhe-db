//! 数値列（INTEGER/BIGINT/REAL/DOUBLE）の `GROUP BY` キー・式内参照・精度境界と
//! TEXT 列の範囲比較（Issue #1183。ポインタ: `docs/spec/04-behavior/sql-surface.md`
//! SQL-24・SQL-26、`docs/spec/04-behavior/data-model.md` TABLE-13）の結合テスト。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` の production
//! 経路で検証する。RLS 境界（他テナントの行が結果・件数・グループに現れない）は
//! tenant-b の行を `Visibility::Private` で投入して確認する（`Public` は全テナントに
//! 見えるため境界の検証にならない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

type Row = (
    u64,
    &'static str,
    Option<i32>,
    Option<i64>,
    Option<f32>,
    Option<f64>,
);

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("total", ColumnType::BigInt, true),
            ColumnDef::new("ratio", ColumnType::Real, true),
            ColumnDef::new("score", ColumnType::Double, true),
        ],
    )
}

fn ctx_a() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("tenant-a")
}

fn ctx_b() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
        .expect("tenant-b")
}

fn seed(rows_a: &[Row], rows_b: &[Row]) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("numeric-column-predicates");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (ctx, vis, rows) in [
        (ctx_a(), Visibility::Public, rows_a),
        (ctx_b(), Visibility::Private, rows_b),
    ] {
        for &(id, lang, qty, total, ratio, score) in rows {
            let op = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("operation id");
            engine::tenant::insert_typed_row(
                &storage,
                TABLE,
                &ctx,
                id,
                vis,
                &[
                    Value::Vector(vec![0.1, 0.2]),
                    Value::Text(lang.to_string()),
                    qty.map_or(Value::Null, Value::Integer),
                    total.map_or(Value::Null, Value::BigInt),
                    ratio.map_or(Value::Null, Value::Real),
                    score.map_or(Value::Null, Value::Double),
                ],
                &op,
            )
            .expect("insert row");
        }
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn rows_a() -> Vec<Row> {
    vec![
        (1, "alpha", Some(1), Some(10), Some(0.5), Some(-0.0)),
        (2, "beta", Some(1), Some(20), Some(1.5), Some(0.0)),
        (3, "gamma", Some(3), Some(-30), Some(1.5), Some(2.5)),
        (4, "delta", None, None, None, None),
        (5, "epsilon", None, Some(50), Some(5.0), Some(2.5)),
    ]
}

/// tenant-b の行。どの述語・キーにも一致しうる値だが、tenant-a からは見えない。
fn rows_b() -> Vec<Row> {
    vec![
        (100, "alpha", Some(1), Some(10), Some(0.5), Some(0.0)),
        (101, "zzz", Some(7), Some(70), Some(7.0), Some(7.5)),
    ]
}

fn cells(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Vec<Cell>> {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed, got {e:?}"))
        .rows
        .into_iter()
        .map(|r| r.cells)
        .collect()
}

#[test]
fn group_by_integer_key_sorts_ascending_with_null_last_and_hides_other_tenants() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT qty, COUNT(*) FROM {TABLE} GROUP BY qty"),
    );
    assert_eq!(
        out,
        vec![
            vec![Cell::SignedInteger(1), Cell::Integer(2)],
            vec![Cell::SignedInteger(3), Cell::Integer(1)],
            vec![Cell::Null, Cell::Integer(2)],
        ],
        "tenant-b の qty=1・qty=7 はグループにも件数にも現れない"
    );
}

#[test]
fn group_by_order_by_key_desc_puts_null_first() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT qty, COUNT(*) FROM {TABLE} GROUP BY qty ORDER BY qty DESC"),
    );
    assert_eq!(
        out,
        // 降順は PostgreSQL と同じく NULL が先頭（Issue #1185・SQL-25 (d) の契約）。
        vec![
            vec![Cell::Null, Cell::Integer(2)],
            vec![Cell::SignedInteger(3), Cell::Integer(1)],
            vec![Cell::SignedInteger(1), Cell::Integer(2)],
        ]
    );
}

#[test]
fn group_by_bigint_real_and_double_keys() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);

    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT total, COUNT(*) FROM {TABLE} GROUP BY total"),
    );
    assert_eq!(out.len(), 5, "BIGINT 値 4 種類＋NULL");
    assert_eq!(out[0], vec![Cell::SignedInteger(-30), Cell::Integer(1)]);
    assert_eq!(out[4], vec![Cell::Null, Cell::Integer(1)]);

    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT ratio, COUNT(*) FROM {TABLE} GROUP BY ratio"),
    );
    assert_eq!(
        out,
        vec![
            vec![Cell::Float(0.5), Cell::Integer(1)],
            vec![Cell::Float(1.5), Cell::Integer(2)],
            vec![Cell::Float(5.0), Cell::Integer(1)],
            vec![Cell::Null, Cell::Integer(1)],
        ]
    );

    // `-0.0` と `0.0` は同じグループ（PostgreSQL と同じ）。
    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT score, COUNT(*) FROM {TABLE} GROUP BY score"),
    );
    assert_eq!(
        out,
        vec![
            vec![Cell::Float(0.0), Cell::Integer(2)],
            vec![Cell::Float(2.5), Cell::Integer(2)],
            vec![Cell::Null, Cell::Integer(1)],
        ]
    );
}

#[test]
fn group_by_text_and_numeric_composite_key() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT lang, qty, COUNT(*) FROM {TABLE} WHERE qty >= 1 GROUP BY lang, qty"),
    );
    assert_eq!(
        out,
        vec![
            vec![
                Cell::Text("alpha".into()),
                Cell::SignedInteger(1),
                Cell::Integer(1)
            ],
            vec![
                Cell::Text("beta".into()),
                Cell::SignedInteger(1),
                Cell::Integer(1)
            ],
            vec![
                Cell::Text("gamma".into()),
                Cell::SignedInteger(3),
                Cell::Integer(1)
            ],
        ]
    );
}

#[test]
fn group_by_numeric_key_is_stable_across_repeated_runs_with_index_cache() {
    // 同じクエリを繰り返しても（スカラー索引キャッシュが温まっても）結果が
    // 変わらない（数値キーは TEXT 辞書索引経路へ入らない）。
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    let sql = format!("SELECT qty, COUNT(*) FROM {TABLE} WHERE lang > 'a' GROUP BY qty");
    let first = cells(&core, &ctx_a(), &sql);
    for _ in 0..3 {
        assert_eq!(cells(&core, &ctx_a(), &sql), first);
    }
    assert_eq!(first.len(), 3);
}

#[test]
fn group_by_rejects_unsupported_key_types_with_22000() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    // 疑似列 `id` は Issue #1185 で `GROUP BY` キーとして受理される。
    for key in ["embedding", "nope"] {
        let err = core
            .execute_sql(
                &ctx_a(),
                &format!("SELECT {key}, COUNT(*) FROM {TABLE} GROUP BY {key}"),
            )
            .expect_err("unsupported key must be rejected");
        assert_eq!(err.wire_code(), "22000", "{key}");
    }
}

#[test]
fn numeric_and_text_range_predicates_never_match_other_tenants() {
    let (core, path) = seed(&rows_a(), &rows_b());
    let _guard = CleanupGuard(path);
    for where_clause in [
        "qty > 5",
        "total >= 70",
        "ratio > 6.5",
        "score > 7",
        "lang > 'yyy'",
        "NOT lang < 'zzz'",
    ] {
        let out = cells(
            &core,
            &ctx_a(),
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        );
        assert!(out.is_empty(), "{where_clause} must not see tenant-b rows");
        let count = cells(
            &core,
            &ctx_a(),
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {where_clause}"),
        );
        assert_eq!(count, vec![vec![Cell::Integer(0)]], "{where_clause}");
        // tenant-b 自身には見える（述語自体は正しく評価されている）。
        let own = cells(
            &core,
            &ctx_b(),
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        );
        assert_eq!(own, vec![vec![Cell::Integer(101)]], "{where_clause}");
    }
}

#[test]
fn bigint_beyond_exact_f64_range_fails_closed_with_22000() {
    let big = (1i64 << 53) + 1;
    let (core, path) = seed(&[(1, "alpha", Some(1), Some(big), None, None)], &[]);
    let _guard = CleanupGuard(path);
    let err = core
        .execute_sql(
            &ctx_a(),
            &format!("SELECT id FROM {TABLE} WHERE total > 0 LIMIT 10"),
        )
        .expect_err("inexact BIGINT must fail closed");
    assert_eq!(err.wire_code(), "22000");

    // 境界ちょうど（2^53）は受理される。
    let (core, path2) = seed(&[(1, "alpha", Some(1), Some(1i64 << 53), None, None)], &[]);
    let _guard2 = CleanupGuard(path2);
    let out = cells(
        &core,
        &ctx_a(),
        &format!("SELECT id FROM {TABLE} WHERE total > 0 LIMIT 10"),
    );
    assert_eq!(out.len(), 1);
}

#[test]
fn text_range_uses_byte_order_and_null_never_matches() {
    let rows: Vec<Row> = vec![
        (1, "Z", None, None, None, None),
        (2, "a", None, None, None, None),
        (3, "あ", None, None, None, None),
    ];
    let (core, path) = seed(&rows, &[]);
    let _guard = CleanupGuard(path);
    let ids = |where_clause: &str| -> Vec<u64> {
        let mut v: Vec<u64> = cells(
            &core,
            &ctx_a(),
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .into_iter()
        .map(|r| match &r[0] {
            Cell::Integer(v) => *v,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
        v.sort_unstable();
        v
    };
    // バイト順: 'Z'(0x5A) < 'a'(0x61) < 'あ'(0xE3...)
    assert_eq!(ids("lang < 'a'"), vec![1]);
    assert_eq!(ids("lang > 'a'"), vec![3]);
    assert_eq!(ids("lang >= 'a'"), vec![2, 3]);
}

/// CHECK 文脈の TEXT 範囲比較は WHERE 側の書き換え（Issue #1183）の対象外で、従来どおり
/// `22000` で拒否される（fail-closed。永続化・再オープン時の再検証経路が未検証のため）。
/// 同じ条件の WHERE は受理される。
#[test]
fn check_constraint_text_range_compare_is_rejected_while_where_is_accepted() {
    let (core, path) = seed(&[(1, "b", None, None, None, None)], &[]);
    let _guard = CleanupGuard(path);
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();

    let err = core
        .execute_sql_in_session(
            &ctx_a(),
            &mut session,
            "CREATE TABLE chk (lang TEXT CHECK (lang > 'a'))",
        )
        .expect_err("TEXT range compare in CHECK must be rejected");
    assert_eq!(err.wire_code(), "22000");

    // WHERE 側は従来どおり受理される。
    let rows = cells(
        &core,
        &ctx_a(),
        &format!("SELECT id FROM {TABLE} WHERE lang > 'a' LIMIT 10"),
    );
    assert_eq!(rows.len(), 1);
}
