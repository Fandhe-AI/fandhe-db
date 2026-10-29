//! ウィンドウ関数を含む `SELECT` の `EXPLAIN` 結合テスト（Issue #1896・SQL-27・SQL-64 (6)・
//! RLS-10 ポインタ）。
//!
//! 窓付き広域取得は `scalar_plan:`／`access_path:` の 2 行の後ろへ、窓項目ごとに
//! `window[i]: <func> partition_keys=<n> order_keys=<m>` を追記する。本体は実行せず、
//! 出力はテナント・可視性・行数に依存しない。対象外の形（値の部分クエリ・CTE 等）は
//! 従来どおり fail-closed で拒否する。入口は `execute_sql_in_session`（`execute_sql` は
//! EXPLAIN を `42601` で拒否する既存契約）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn make_core(name: &str, rows: u64) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    for id in 1..=rows {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("w-explain-{name}-{id}"))
                .expect("op id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx("tenant-a"),
            id,
            Visibility::Private,
            &[
                Value::Text(if id % 2 == 0 { "ja" } else { "en" }.to_string()),
                Value::Integer(i32::try_from(id).expect("id fits")),
            ],
            &op_id,
        )
        .expect("insert");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

#[test]
fn basic_window_stage_row() {
    let (core, _g) = make_core("wexp-basic", 4);
    assert_eq!(
        lines(
            &core,
            "tenant-a",
            "EXPLAIN SELECT id, row_number() OVER (PARTITION BY lang ORDER BY score) FROM docs LIMIT 10"
        ),
        vec![
            "scalar_plan: plain_scan",
            "access_path: full_scan",
            "window[0]: row_number partition_keys=1 order_keys=1",
        ]
    );
}

#[test]
fn multiple_items_in_order_and_closed_vocabulary() {
    let (core, _g) = make_core("wexp-multi", 4);
    let sql = "EXPLAIN SELECT id, sum(score) OVER (), lag(score) OVER (ORDER BY score), \
               rank() OVER (PARTITION BY lang, score ORDER BY score DESC, id), \
               dense_rank() OVER (ORDER BY id), count(*) OVER (), avg(score) OVER (), \
               min(score) OVER (), max(score) OVER (), lead(score) OVER (ORDER BY id), \
               first_value(score) OVER (ORDER BY id), last_value(score) OVER (ORDER BY id), \
               nth_value(score, 2) OVER (ORDER BY id), ntile(2) OVER (ORDER BY id), \
               percent_rank() OVER (ORDER BY id), cume_dist() OVER (ORDER BY id) \
               FROM docs LIMIT 10";
    let out = lines(&core, "tenant-a", sql);
    let expected = [
        "window[0]: sum partition_keys=0 order_keys=0",
        "window[1]: lag partition_keys=0 order_keys=1",
        "window[2]: rank partition_keys=2 order_keys=2",
        "window[3]: dense_rank partition_keys=0 order_keys=1",
        "window[4]: count partition_keys=0 order_keys=0",
        "window[5]: avg partition_keys=0 order_keys=0",
        "window[6]: min partition_keys=0 order_keys=0",
        "window[7]: max partition_keys=0 order_keys=0",
        "window[8]: lead partition_keys=0 order_keys=1",
        "window[9]: first_value partition_keys=0 order_keys=1",
        "window[10]: last_value partition_keys=0 order_keys=1",
        "window[11]: nth_value partition_keys=0 order_keys=1",
        "window[12]: ntile partition_keys=0 order_keys=1",
        "window[13]: percent_rank partition_keys=0 order_keys=1",
        "window[14]: cume_dist partition_keys=0 order_keys=1",
    ];
    assert_eq!(out.len(), 2 + expected.len());
    assert_eq!(&out[2..], &expected);
}

#[test]
fn frame_filter_order_by_offset_do_not_change_row_format() {
    let (core, _g) = make_core("wexp-misc", 4);
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT id, sum(score) FILTER (WHERE score > 1) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
         FROM docs ORDER BY id LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        out,
        vec![
            "scalar_plan: plain_scan",
            "access_path: full_scan",
            "window[0]: sum partition_keys=0 order_keys=1",
        ]
    );
}

#[test]
fn named_window_matches_inline() {
    let (core, _g) = make_core("wexp-named", 3);
    let named = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT id, rank() OVER w FROM docs WINDOW w AS (PARTITION BY lang ORDER BY score) LIMIT 10",
    );
    let inline = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT id, rank() OVER (PARTITION BY lang ORDER BY score) FROM docs LIMIT 10",
    );
    assert_eq!(named, inline);
}

#[test]
fn body_is_not_executed() {
    let (core, _g) = make_core("wexp-noexec", 3);
    // 実行すると 22004／22013 になるフレームオフセットでも EXPLAIN は成功する。
    let sql_null = "SELECT id, sum(score) OVER (ORDER BY id ROWS BETWEEN NULL PRECEDING AND CURRENT ROW) FROM docs LIMIT 10";
    assert!(run(&core, "tenant-a", sql_null).is_err());
    let out = lines(&core, "tenant-a", &format!("EXPLAIN {sql_null}"));
    assert_eq!(out.len(), 3);
    let sql_neg = "SELECT id, sum(score) OVER (ORDER BY id ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM docs LIMIT 10";
    assert!(run(&core, "tenant-a", sql_neg).is_err());
    assert_eq!(
        lines(&core, "tenant-a", &format!("EXPLAIN {sql_neg}")).len(),
        3
    );
}

#[test]
fn output_is_independent_of_rows_and_tenant() {
    let sql = "EXPLAIN SELECT id, row_number() OVER (ORDER BY score) FROM docs LIMIT 10";
    let (empty, _g1) = make_core("wexp-empty", 0);
    let (many, _g2) = make_core("wexp-many", 20);
    let a = lines(&empty, "tenant-a", sql);
    assert_eq!(a, lines(&many, "tenant-a", sql));
    assert_eq!(a, lines(&many, "tenant-b", sql));
    let mut session = SessionState::default();
    let restricted =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public]).expect("valid tenant");
    match many
        .execute_sql_in_session(&restricted, &mut session, sql)
        .expect("explain")
    {
        SqlOutcome::Explain(r) => assert_eq!(r.rows.len(), a.len()),
        other => panic!("expected Explain, got {other:?}"),
    }
}

#[test]
fn error_codes_match_non_explain() {
    let (core, _g) = make_core("wexp-err", 3);
    let cases = [
        "SELECT id, row_number() OVER (ORDER BY score) AS rn FROM docs WHERE rn > 1 LIMIT 10",
        "SELECT id, row_number() OVER (ORDER BY score) AS rn FROM docs ORDER BY rn LIMIT 10",
        "SELECT id, row_number() OVER (ORDER BY nope) FROM docs LIMIT 10",
        "SELECT id, sum(lang) OVER () FROM docs LIMIT 10",
        "SELECT id, rank() OVER (PARTITION BY lang, lang, lang, lang, lang, lang, lang, lang, lang) FROM docs LIMIT 10",
        "SELECT id, rank() OVER nosuch FROM docs LIMIT 10",
    ];
    for sql in cases {
        let plain = code(&core, sql);
        let explain = code(&core, &format!("EXPLAIN {sql}"));
        assert_eq!(plain, explain, "sql={sql}");
    }
}

#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (core, _g) = make_core("wexp-closed", 3);
    for sql in [
        "EXPLAIN SELECT id, row_number() OVER (ORDER BY score), (SELECT 1) FROM docs LIMIT 10",
        "EXPLAIN WITH c AS (SELECT id, row_number() OVER (ORDER BY score) AS rn FROM docs LIMIT 10) SELECT id FROM c LIMIT 10",
        "EXPLAIN SELECT lang, count(*) OVER () FROM docs GROUP BY lang",
    ] {
        assert_eq!(code(&core, sql), "42601", "sql={sql}");
    }
    // セッション非対応の入口は従来どおり EXPLAIN を 42601 で拒否する。
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT id, row_number() OVER () FROM docs LIMIT 10",
        )
        .expect_err("execute_sql rejects EXPLAIN");
    assert_eq!(err.wire_code(), "42601");
}
