//! `BIT[]`／`BIT(n)[]` 列（`ArrayElemType::Bit`。TABLE-14・TABLE-35・TABLE-48・Issue #3235）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、カタログへの永続化と再オープン後の復元、
//! INSERT／SELECT／UPDATE／UPSERT／DEFAULT／ALTER ADD COLUMN の往復、文字種・長さ・要素数の
//! 拒否（副作用なし）、添字・スライス、未解禁の機能（比較・並べ替え・集計・CAST・構築子）の fail-closed、
//! 同一宣言の代入、UNIQUE、テナント境界を固定する。`VARBIT[]`／`BIT VARYING[]` はスカラーの VARBIT が
//! 未実装のため `0A000` のまま（`sql_bit_column` の型宣言境界テスト）。

use engine::bit_string::BitString;
use engine::catalog::{ArrayElemType, ColumnType};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue as V;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn select(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn first_cell(core: &EngineCore, sql: &str) -> Cell {
    select(core, "alice", sql)
        .rows
        .first()
        .and_then(|r| r.cells.first())
        .cloned()
        .unwrap_or_else(|| panic!("{sql} returned no cell"))
}

fn bits(items: &[Option<&str>]) -> V {
    V::Bit(
        items
            .iter()
            .map(|o| o.map(|s| BitString::parse_text(s, s.len() as u32).expect("bit")))
            .collect(),
    )
}

fn arr(core: &EngineCore, sql: &str) -> V {
    match first_cell(core, sql) {
        Cell::Array(a) => a,
        other => panic!("{sql}: expected an array, got {other:?}"),
    }
}

fn count(core: &EngineCore, tenant: &str, sql: &str) -> usize {
    select(core, tenant, sql).rows.len()
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    Storage::open(path)
        .expect("reopen")
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4)[], w BIT(4)[], m BIT(4)[2], c BIT[], t TEXT[])",
    );
    ok(
        &core,
        "INSERT INTO a (id, v, w, c, t) VALUES (1, '{1010,NULL,0101}', '{1111}', '{1,0}', '{x}') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO a (id, v) VALUES (2, '{}') USING OPERATION_ID 'i2'",
    );
    (path, guard, core)
}

#[test]
fn type_is_persisted_and_values_survive_reopen() {
    let (path, _g, core) = setup("ba-persist");
    drop(core);
    for (col, n) in [("v", 4u32), ("m", 4), ("c", 1)] {
        match column_type(&path, "a", col) {
            ColumnType::Array(a) => assert_eq!(a.elem(), ArrayElemType::Bit(n), "{col}"),
            other => panic!("{col}: expected a bit array, got {other:?}"),
        }
    }
    let core = core_at(&path);
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 1 LIMIT 1"),
        bits(&[Some("1010"), None, Some("0101")])
    );
    assert_eq!(
        arr(&core, "SELECT c FROM a WHERE id = 1 LIMIT 1"),
        bits(&[Some("1"), Some("0")])
    );
}

#[test]
fn insert_select_roundtrip_covers_null_empty_multidim_and_partial_bytes() {
    let (_p, _g, core) = setup("ba-roundtrip");
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 2 LIMIT 1"),
        V::Bit(vec![])
    );
    ok(
        &core,
        "CREATE TABLE md (id BIGINT PRIMARY KEY, v BIT(2)[][], n BIT(9)[])",
    );
    ok(
        &core,
        "INSERT INTO md (id, v, n) VALUES (3, '{{10,01},{11,00}}', '{101010101,000000001}') USING OPERATION_ID 'i3'",
    );
    match arr(&core, "SELECT v FROM md WHERE id = 3 LIMIT 1") {
        V::Multi(m) => assert_eq!(m.dims(), &[2, 2]),
        other => panic!("expected a multi-dimensional array, got {other:?}"),
    }
    assert_eq!(
        arr(&core, "SELECT n FROM md WHERE id = 3 LIMIT 1"),
        bits(&[Some("101010101"), Some("000000001")])
    );
}

#[test]
fn update_upsert_returning_default_and_alter_work() {
    let (_p, _g, core) = setup("ba-dml");
    ok(
        &core,
        "UPDATE a SET v = '{0001,1000}' WHERE id = 2 USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 2 LIMIT 1"),
        bits(&[Some("0001"), Some("1000")])
    );
    ok(
        &core,
        "INSERT INTO a (id, v) VALUES (2, '{1100}') ON CONFLICT (id) DO UPDATE SET v = '{0011}' USING OPERATION_ID 'u2'",
    );
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 2 LIMIT 1"),
        bits(&[Some("0011")])
    );
    ok(
        &core,
        "INSERT INTO a (id, v) VALUES (5, '{1001}') RETURNING v USING OPERATION_ID 'r1'",
    );
    ok(
        &core,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, v BIT(4)[] DEFAULT '{1010}')",
    );
    ok(
        &core,
        "INSERT INTO d (id) VALUES (1) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        arr(&core, "SELECT v FROM d WHERE id = 1 LIMIT 1"),
        bits(&[Some("1010")])
    );
    ok(&core, "ALTER TABLE d ADD COLUMN z BIT(4)[]");
    assert_eq!(
        first_cell(&core, "SELECT z FROM d WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
}

#[test]
fn malformed_oversized_and_mismatched_literals_are_rejected_without_side_effects() {
    let (_p, _g, core) = setup("ba-reject");
    for (lit, want) in [
        ("'{10x0}'", "22P02"),
        ("'{101}'", "22026"),
        ("'{10101}'", "22026"),
        ("'{1010,1010,1010}'", "54000"),
    ] {
        let sql = format!("INSERT INTO a (id, m) VALUES (9, {lit}) USING OPERATION_ID 'bad'");
        assert_eq!(code(&core, &sql), want, "{sql}");
    }
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE id = 9 LIMIT 10"),
        0
    );
    // 巨大な宣言長でも、長さの合わない入力は確保の前に `22026` で拒否する。
    ok(
        &core,
        "CREATE TABLE big (id BIGINT PRIMARY KEY, v BIT(33554392)[])",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO big (id, v) VALUES (1, '{0000}') USING OPERATION_ID 'big1'"
        ),
        "22026"
    );
}

#[test]
fn subscript_and_slice_return_bit_values() {
    let (_p, _g, core) = setup("ba-subscript");
    let r = select(
        &core,
        "alice",
        "SELECT v[1], v[1:2], v FROM a WHERE id = 1 LIMIT 1",
    );
    let tys: Vec<Option<ColumnType>> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tys[0], Some(ColumnType::Bit(4)));
    for ty in &tys[1..] {
        match ty {
            Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::Bit(4)),
            other => panic!("expected bit(4)[], got {other:?}"),
        }
    }
    match first_cell(&core, "SELECT v[1] FROM a WHERE id = 1 LIMIT 1") {
        Cell::Bit(b) => assert_eq!(b.format_text(), "1010"),
        other => panic!("expected a bit cell, got {other:?}"),
    }
    assert_eq!(
        arr(&core, "SELECT v[1:2] FROM a WHERE id = 1 LIMIT 1"),
        bits(&[Some("1010"), None])
    );
    assert_eq!(
        first_cell(&core, "SELECT v[9] FROM a WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
    assert_eq!(
        first_cell(&core, "SELECT v[2] FROM a WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
}

/// 比較・包含・量化・並べ替え・集計・構築子・CAST・索引・型変更は、スカラー BIT と同じく未解禁で
/// fail-closed（`0A000` 等）。拒否後も行は変わらない。
#[test]
fn unreleased_features_stay_fail_closed() {
    let (_p, _g, core) = setup("ba-failclosed");
    for sql in [
        "SELECT id FROM a WHERE v @> '{1010}' LIMIT 10",
        "SELECT id FROM a WHERE v && '{1010}' LIMIT 10",
        "SELECT id FROM a WHERE '1010' = ANY(v) LIMIT 10",
        "SELECT id FROM a WHERE v[1] = '1010' LIMIT 10",
        "SELECT id FROM a ORDER BY v LIMIT 10",
        "SELECT DISTINCT v FROM a LIMIT 10",
        "SELECT v, COUNT(*) FROM a GROUP BY v LIMIT 10",
        "SELECT array_agg(v[1]) FROM a",
        "SELECT ARRAY[v[1], v[1]] FROM a LIMIT 1",
        "SELECT CAST(v AS TEXT) FROM a LIMIT 1",
        "SELECT v::TEXT FROM a LIMIT 1",
        "SELECT '{1010}'::BIT(4)[] FROM a LIMIT 1",
        "SELECT CAST(t AS BIT(4)[]) FROM a LIMIT 1",
        "CREATE INDEX ix ON a (v)",
        "ALTER TABLE a ALTER COLUMN v TYPE TEXT[]",
        "ALTER TABLE a ALTER COLUMN t TYPE BIT(4)[]",
    ] {
        match exec(&core, sql) {
            Ok(_) => panic!("{sql} must be rejected"),
            Err(e) => assert!(
                matches!(e.wire_code(), "0A000" | "42804" | "22000"),
                "{sql}: unexpected code {}",
                e.wire_code()
            ),
        }
    }
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 1 LIMIT 1"),
        bits(&[Some("1010"), None, Some("0101")])
    );
}

/// 同じ宣言どうしの値の等価（`=`・`<>`・集合演算の重複除去）は、ビット長を含む正準ペイロードで
/// 判定するため受理する。順序比較は未解禁。
#[test]
fn equality_is_length_aware_and_ordering_is_rejected() {
    let (_p, _g, core) = setup("ba-eq");
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE v = '{1010,NULL,0101}' LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE v = '{1010}' LIMIT 10"
        ),
        0
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE v <> '{1010}' LIMIT 10"
        ),
        2
    );
    // 長さの違う要素は束縛段で拒否する（`bit(4)` へ `bit(8)` を暗黙に合わせない）。
    assert_eq!(
        code(&core, "SELECT id FROM a WHERE v = '{10100000}' LIMIT 10"),
        "22026"
    );
    assert!(exec(&core, "SELECT id FROM a WHERE v < '{1010}' LIMIT 10").is_err());
}

/// 式経路（列どうし・型なしリテラルを左右どちらに置く形）の等値比較も、列対リテラルと同じく
/// 受理して正準ペイロードで判定する。順序比較・包含は `0A000` のまま（Issue #3235）。
#[test]
fn expression_equality_between_columns_and_literals_is_accepted() {
    let (_p, _g, core) = setup("ba-expr-eq");
    ok(
        &core,
        "INSERT INTO a (id, v, w) VALUES (3, '{1111}', '{1111}') USING OPERATION_ID 'i3'",
    );
    let n = |sql: &str| count(&core, "alice", sql);
    assert_eq!(n("SELECT id FROM a WHERE v = w LIMIT 10"), 1);
    assert_eq!(n("SELECT id FROM a WHERE v <> w LIMIT 10"), 1);
    assert_eq!(n("SELECT id FROM a WHERE v = '{1111}' LIMIT 10"), 1);
    assert_eq!(n("SELECT id FROM a WHERE '{1111}' = v LIMIT 10"), 1);
    assert_eq!(
        n("SELECT id FROM a WHERE '{1010,NULL,0101}' = v LIMIT 10"),
        1
    );
    assert_eq!(n("SELECT id FROM a WHERE NOT (v = w) LIMIT 10"), 1);
    // 宣言の長さが異なる配列どうし・長さの違う要素のリテラルは拒否する。
    assert!(exec(&core, "SELECT id FROM a WHERE v = c LIMIT 10").is_err());
    assert_eq!(
        code(&core, "SELECT id FROM a WHERE '{10100000}' = v LIMIT 10"),
        "22026"
    );
    for sql in [
        "SELECT id FROM a WHERE v < w LIMIT 10",
        "SELECT id FROM a WHERE v >= w LIMIT 10",
        "SELECT id FROM a WHERE v @> w LIMIT 10",
        "SELECT id FROM a WHERE v && w LIMIT 10",
        "SELECT id FROM a WHERE v <@ '{1111}' LIMIT 10",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
}

#[test]
fn identical_declaration_assignment_is_accepted_and_others_are_rejected() {
    let (_p, _g, core) = setup("ba-assign");
    ok(
        &core,
        "UPDATE a SET v = w WHERE id = 1 USING OPERATION_ID 'as1'",
    );
    assert_eq!(
        arr(&core, "SELECT v FROM a WHERE id = 1 LIMIT 1"),
        bits(&[Some("1111")])
    );
    ok(
        &core,
        "CREATE TABLE b (id BIGINT PRIMARY KEY, v BIT(4)[], x BIT(8)[], y TEXT[])",
    );
    ok(
        &core,
        "INSERT INTO b (id, v) SELECT id, w FROM a WHERE id = 1 USING OPERATION_ID 'as2'",
    );
    for sql in [
        "UPDATE b SET x = v WHERE id = 1 USING OPERATION_ID 'as3'",
        "UPDATE b SET y = v WHERE id = 1 USING OPERATION_ID 'as4'",
        "UPDATE a SET v = t WHERE id = 1 USING OPERATION_ID 'as5'",
    ] {
        let c = code(&core, sql);
        assert!(
            matches!(c.as_str(), "0A000" | "42804" | "22026"),
            "{sql}: unexpected code {c}"
        );
    }
}

#[test]
fn unique_bit_array_rejects_duplicates() {
    let (_p, _g, core) = setup("ba-uniq");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, v BIT(4)[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (1, '{1010,0101}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (2, '{0101,1010}') USING OPERATION_ID 'u2'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, v) VALUES (3, '{1010,0101}') USING OPERATION_ID 'u3'"
        ),
        "23505"
    );
    // NULL 要素を含む配列どうしの重複も検出する。
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (4, '{1010,NULL}') USING OPERATION_ID 'u4'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, v) VALUES (5, '{1010,NULL}') USING OPERATION_ID 'u5'"
        ),
        "23505"
    );
}

#[test]
fn other_tenants_see_no_rows() {
    let (_p, _g, core) = setup("ba-tenant");
    assert_eq!(count(&core, "bob", "SELECT id FROM a LIMIT 10"), 0);
}
