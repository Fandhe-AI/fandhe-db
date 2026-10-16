//! 列の照合順序（`COLLATE "C"`／`"POSIX"`／指定なし）に対する比較・`ORDER BY`・`GROUP BY`・
//! `DISTINCT`・`MIN`／`MAX`・スカラー二次索引の結合テスト（TABLE-28、Issue #1610）。
//!
//! 照合ごとの列（`t0` 指定なし・`tc` `C`・`vp` `POSIX`・`bc` `CHAR` の `C`）に同じ値集合を
//! 入れ、索引経路・全走査（`id + 0 > 0` 追加で索引を無効化）・素の形・Rust 側のバイト列順
//! オラクルの 4 者が完全一致することを固定する（受け入れ条件: 索引あり／なしで結果が一致）。
//! 照合だけの `ALTER`（カタログのみ）の直後と再起動後も一致すること、他テナントの Private 行が
//! 結果に現れないこと（RLS）も確認する。

use std::collections::BTreeMap;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 64;
const OTHER_TENANT_ID: u64 = 900;
const CHAR_WIDTH: usize = 6;
/// バイト列順が大小文字・多バイト・接頭辞の順で固定される値プール。
const POOL: [&str; 8] = ["B", "a", "ab", "", "é", "あ", "A", "b"];
const COLUMNS: [&str; 4] = ["t0", "tc", "vp", "bc"];

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("t0", ColumnType::Text, None),
            col("tc", ColumnType::Text, Some(Collation::C)),
            col("vp", ColumnType::Varchar(Some(20)), Some(Collation::Posix)),
            col(
                "bc",
                ColumnType::Bpchar(CHAR_WIDTH as u32),
                Some(Collation::C),
            ),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id % 7 == 0` は NULL、それ以外は値プールから決定的に選ぶ。
fn value_of(id: u64) -> Option<&'static str> {
    if id.is_multiple_of(7) {
        None
    } else {
        POOL.get((id % 8) as usize).copied()
    }
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, v: Option<&str>, vis: Visibility) {
    let text = |pad: bool| match v {
        Some(s) if pad => Value::Text(format!("{s:<CHAR_WIDTH$}")),
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            text(false),
            text(false),
            text(false),
            text(true),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core_at(path: &std::path::Path, seed: bool) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    if seed {
        storage.create_table(&schema()).expect("create table");
        for id in 1..=ROWS {
            insert_row(&storage, "tenant-a", id, value_of(id), Visibility::Public);
        }
        // 他テナントの Private 行（どの述語にも合い、ソートで先頭に来る空文字）。
        insert_row(
            &storage,
            "tenant-b",
            OTHER_TENANT_ID,
            Some(""),
            Visibility::Private,
        );
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

fn sorted_ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    assert!(!v.contains(&OTHER_TENANT_ID), "other tenant row leaked");
    v
}

fn ordered_ids(result: &QueryResult) -> Vec<u64> {
    assert!(
        result.rows.iter().all(|r| r.id != OTHER_TENANT_ID),
        "other tenant row leaked"
    );
    result.rows.iter().map(|r| r.id).collect()
}

fn oracle(pred: impl Fn(&str) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(&pred))
        .collect()
}

/// 述語を索引経路・全走査強制・素の形で実行し、オラクルと一致することを固定する。
fn assert_parity(core: &EngineCore, pred: &str, expected: &[u64], expect_index: bool) {
    let vec_sql =
        format!("SELECT id FROM {TABLE} WHERE {pred} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200");
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(sorted_ids(&cold), sorted_ids(&hot), "cold/hot: {pred}");
    assert_eq!(sorted_ids(&hot), expected, "oracle: {pred}");
    if expect_index && !expected.is_empty() && expected.len() * 2 <= ROWS as usize {
        assert!(after > before, "index not consumed: {pred}");
    }
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    assert_eq!(
        sorted_ids(&run(core, &scan_sql)),
        expected,
        "full scan: {pred}"
    );
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 200");
    assert_eq!(
        sorted_ids(&run(core, &plain_sql)),
        expected,
        "plain: {pred}"
    );
}

fn lit(s: &str) -> String {
    format!("'{s}'")
}

fn predicate_checks(core: &EngineCore, col: &str) {
    for needle in ["a", "B", "", "é", "zz"] {
        assert_parity(
            core,
            &format!("{col} = {}", lit(needle)),
            &oracle(|v| v == needle),
            true,
        );
    }
    assert_parity(
        core,
        &format!("{col} IN ({}, {})", lit("a"), lit("é")),
        &oracle(|v| v == "a" || v == "é"),
        true,
    );
    if col != "bc" {
        assert_parity(
            core,
            &format!("{col} LIKE 'a%'"),
            &oracle(|v| v.starts_with('a')),
            true,
        );
    }
    // 範囲比較（`<`・`>=`・`BETWEEN`）は TEXT 列の宣言的述語としては未対応（既存挙動）。
}

/// バイト列昇順（NULL は最後）、降順（NULL は最初）、同値は id 昇順。
fn oracle_order(desc: bool) -> Vec<u64> {
    let mut rows: Vec<(Option<&str>, u64)> = (1..=ROWS).map(|id| (value_of(id), id)).collect();
    rows.sort_by(|a, b| {
        let key = |x: &Option<&str>| match x {
            Some(s) => (0u8, s.as_bytes().to_vec()),
            None => (1u8, Vec::new()),
        };
        let ord = key(&a.0).cmp(&key(&b.0));
        let ord = if desc { ord.reverse() } else { ord };
        ord.then(a.1.cmp(&b.1))
    });
    rows.into_iter().map(|(_, id)| id).collect()
}

fn text_of(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Text(s) => Some(s.trim_end_matches(' ').to_string()),
        other => panic!("unexpected cell: {other:?}"),
    }
}

fn count_of(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(n) => *n,
        Cell::SignedInteger(n) => u64::try_from(*n).expect("non-negative count"),
        other => panic!("unexpected count cell: {other:?}"),
    }
}

fn order_group_checks(core: &EngineCore, col: &str) {
    for (desc, dir) in [(false, "ASC"), (true, "DESC")] {
        let expected = oracle_order(desc);
        let plain = ordered_ids(&run(
            core,
            &format!("SELECT id FROM {TABLE} ORDER BY {col} {dir}, id LIMIT 200"),
        ));
        assert_eq!(plain, expected, "ORDER BY {col} {dir}");
        for name in ["C", "POSIX"] {
            let with_collate = ordered_ids(&run(
                core,
                &format!(
                    "SELECT id FROM {TABLE} ORDER BY {col} COLLATE \"{name}\" {dir}, id LIMIT 200"
                ),
            ));
            assert_eq!(
                with_collate, expected,
                "ORDER BY {col} COLLATE {name} {dir}"
            );
        }
    }

    // GROUP BY（単一 TEXT キー〔索引の column_groups 経路〕）。
    let mut want: BTreeMap<Option<String>, u64> = BTreeMap::new();
    for id in 1..=ROWS {
        *want.entry(value_of(id).map(str::to_string)).or_default() += 1;
    }
    let sql = format!("SELECT {col}, COUNT(*) FROM {TABLE} GROUP BY {col}");
    let res = run(core, &sql);
    let got: BTreeMap<Option<String>, u64> = res
        .rows
        .iter()
        .map(|r| (text_of(&r.cells[0]), count_of(&r.cells[1])))
        .collect();
    assert_eq!(got, want, "{sql}");

    let distinct = run(core, &format!("SELECT DISTINCT {col} FROM {TABLE}"));
    let mut got: Vec<Option<String>> = distinct.rows.iter().map(|r| text_of(&r.cells[0])).collect();
    got.sort();
    let mut want_keys: Vec<Option<String>> = want.keys().cloned().collect();
    want_keys.sort();
    assert_eq!(got, want_keys, "DISTINCT {col}");

    let minmax = run(core, &format!("SELECT MIN({col}), MAX({col}) FROM {TABLE}"));
    let row = minmax.rows.first().expect("one row");
    let vals: Vec<&str> = (1..=ROWS).filter_map(value_of).collect();
    assert_eq!(
        text_of(&row.cells[0]).as_deref(),
        vals.iter().min_by_key(|v| v.as_bytes()).copied(),
        "MIN({col})"
    );
    assert_eq!(
        text_of(&row.cells[1]).as_deref(),
        vals.iter().max_by_key(|v| v.as_bytes()).copied(),
        "MAX({col})"
    );
}

fn all_checks(core: &EngineCore) {
    for col in COLUMNS {
        predicate_checks(core, col);
        // `CHAR` 列の MIN/MAX は未対応（TABLE-31 の後続課題）のため順序・集約は TEXT 系のみ。
        if col != "bc" {
            order_group_checks(core, col);
        }
    }
}

/// TABLE-28: 照合ごとの列で、索引・全走査・オラクルが一致する。
#[test]
fn collated_columns_match_index_scan_and_oracle() {
    let path = unique_db_path("sql-collate-order");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    all_checks(&core);
}

/// TABLE-28: 照合だけの `ALTER`（カタログのみ）の直後も、照合指定なしへ戻した直後も、
/// スカラー索引キャッシュが世代で無効化され結果が一致する。
#[test]
fn collation_alter_keeps_results_consistent() {
    let path = unique_db_path("sql-collate-order-alter");
    let _guard = CleanupGuard(path.clone());
    let core = open_core_at(&path, true);
    all_checks(&core);
    ddl(
        &core,
        "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT COLLATE \"POSIX\"",
    );
    all_checks(&core);
    ddl(&core, "ALTER TABLE docs ALTER COLUMN t0 TYPE TEXT");
    all_checks(&core);
}

/// TABLE-28: 再起動後（同じパスで開き直す）も一致する。
#[test]
fn collated_results_survive_restart() {
    let path = unique_db_path("sql-collate-order-restart");
    let _guard = CleanupGuard(path.clone());
    {
        let core = open_core_at(&path, true);
        all_checks(&core);
    }
    let core = open_core_at(&path, false);
    all_checks(&core);
}
