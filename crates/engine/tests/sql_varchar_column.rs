//! 長さなし `VARCHAR`／`CHARACTER VARYING` 列（`ColumnType::Varchar(None)`。TABLE-31・Issue #2305）の
//! 結合テスト。ポインタ: TABLE-29・TABLE-30・TABLE-31。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログ上 TEXT と区別されて
//! 永続化されること、(2) 値・比較・関数・索引・制約・集合演算・結合が TEXT 列と同じ結果になること、
//! (3) `ALTER COLUMN TYPE` が TEXT と相互変換できること、(4) `VARCHAR[]` が
//! `0A000` のままであること（長さ付き `VARCHAR(n)` は Issue #1563 で受理。`sql_char_length_types.rs`）、を固定する。wire 公告（OID 1043）は wire-server 側のテストが担う。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
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

/// SELECT の行セルを Debug 文字列化して返す（TEXT／VARCHAR のセル表現が同一であることの比較用）。
fn rows(core: &EngineCore, sql: &str) -> Vec<String> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect()
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

/// 同じ内容の TEXT 表 `tt` と VARCHAR 表 `vt` を作る。
fn setup_pair(core: &EngineCore) {
    ok(core, "CREATE TABLE tt (k TEXT, v TEXT)");
    ok(core, "CREATE TABLE vt (k VARCHAR, v CHARACTER VARYING)");
    for (i, (k, v)) in [
        ("apple", "Red"),
        ("apricot", "orange"),
        ("banana", "yellow"),
    ]
    .iter()
    .enumerate()
    {
        for t in ["tt", "vt"] {
            ok(
                core,
                &format!(
                    "INSERT INTO {t} (id, k, v) VALUES ({}, '{k}', '{v}') USING OPERATION_ID '{t}{i}'",
                    i + 1
                ),
            );
        }
    }
}

#[test]
fn unbounded_varchar_is_persisted_as_a_distinct_type() {
    let (path, _g, core) = open("vc-persist");
    ok(
        &core,
        "CREATE TABLE t (a VARCHAR, b character varying, c TEXT)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN d VARCHAR");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Varchar(None));
}

#[test]
fn array_forms_stay_unsupported() {
    let (_p, _g, core) = open("vc-reject");
    ok(&core, "CREATE TABLE t (a INT)");
    for sql in [
        "CREATE TABLE u (a VARCHAR(10)[])",
        "CREATE TABLE u (a VARCHAR[])",
        "CREATE TABLE u (a CHARACTER VARYING[2])",
        "ALTER TABLE t ADD COLUMN b VARCHAR[]",
        "ALTER TABLE t ALTER COLUMN a TYPE VARCHAR[]",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    assert!(exec(&core, "SELECT * FROM u LIMIT 1").is_err());
}

#[test]
fn values_comparisons_and_functions_match_text_columns() {
    let (_p, _g, core) = open("vc-parity");
    setup_pair(&core);
    let queries = [
        "SELECT k, v FROM {t} ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE k = 'apple' LIMIT 10",
        "SELECT k FROM {t} WHERE k <> 'apple' ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE k < 'b' ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE k >= 'apricot' ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE k LIKE 'ap%' ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE k IN ('apple', 'banana') ORDER BY k LIMIT 10",
        "SELECT k FROM {t} WHERE v IS NOT NULL ORDER BY k DESC LIMIT 10",
        "SELECT upper(k), lower(v), length(k) FROM {t} ORDER BY k LIMIT 10",
        "SELECT min(k), max(v), count(k) FROM {t}",
        "SELECT k, count(*) FROM {t} GROUP BY k ORDER BY k LIMIT 10",
    ];
    for q in queries {
        let t = rows(&core, &q.replace("{t}", "tt"));
        let v = rows(&core, &q.replace("{t}", "vt"));
        assert_eq!(t, v, "{q}");
    }
    // UPDATE／DELETE の述語も TEXT と同じ。
    ok(
        &core,
        "UPDATE vt SET v = 'green' WHERE k = 'banana' USING OPERATION_ID 'vu'",
    );
    assert_eq!(
        rows(&core, "SELECT v FROM vt WHERE k = 'banana' LIMIT 10"),
        vec![format!(
            "{:?}",
            vec![engine::sql::exec::Cell::Text("green".into())]
        )]
    );
    ok(
        &core,
        "DELETE FROM vt WHERE k LIKE 'ap%' USING OPERATION_ID 'vd'",
    );
    assert_eq!(rows(&core, "SELECT k FROM vt LIMIT 10").len(), 1);
}

#[test]
fn indexes_and_key_constraints_work_like_text() {
    let (_p, _g, core) = open("vc-keys");
    ok(
        &core,
        "CREATE TABLE t (k VARCHAR PRIMARY KEY, u VARCHAR, UNIQUE (u))",
    );
    ok(&core, "CREATE INDEX idx_u ON t (u)");
    ok(
        &core,
        "INSERT INTO t (id, k, u) VALUES (1, 'a', 'x') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, u) VALUES (2, 'b', 'y') USING OPERATION_ID 'i2'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k, u) VALUES (3, 'a', 'z') USING OPERATION_ID 'i3'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k, u) VALUES (3, 'c', 'y') USING OPERATION_ID 'i4'"
        ),
        "23505"
    );
    assert_eq!(
        rows(&core, "SELECT k FROM t WHERE u = 'y' LIMIT 10").len(),
        1
    );
    assert_eq!(
        rows(&core, "SELECT k FROM t WHERE u LIKE 'x%' LIMIT 10").len(),
        1
    );
}

#[test]
fn text_and_varchar_foreign_keys_reference_each_other() {
    let (_p, _g, core) = open("vc-fk");
    ok(&core, "CREATE TABLE tp (k TEXT PRIMARY KEY)");
    ok(&core, "CREATE TABLE vp (k VARCHAR PRIMARY KEY)");
    ok(
        &core,
        "CREATE TABLE vc (r VARCHAR, FOREIGN KEY (r) REFERENCES tp (k))",
    );
    ok(
        &core,
        "CREATE TABLE tc (r TEXT, FOREIGN KEY (r) REFERENCES vp (k))",
    );
    ok(
        &core,
        "INSERT INTO tp (id, k) VALUES (1, 'a') USING OPERATION_ID 'p1'",
    );
    ok(
        &core,
        "INSERT INTO vp (id, k) VALUES (1, 'a') USING OPERATION_ID 'p2'",
    );
    ok(
        &core,
        "INSERT INTO vc (id, r) VALUES (1, 'a') USING OPERATION_ID 'c1'",
    );
    ok(
        &core,
        "INSERT INTO tc (id, r) VALUES (1, 'a') USING OPERATION_ID 'c2'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO vc (id, r) VALUES (2, 'zz') USING OPERATION_ID 'c3'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO tc (id, r) VALUES (2, 'zz') USING OPERATION_ID 'c4'"
        ),
        "23503"
    );
    assert_eq!(
        code(&core, "DELETE FROM tp WHERE id = 1 USING OPERATION_ID 'd1'"),
        "23503"
    );
    assert_eq!(
        code(&core, "DELETE FROM vp WHERE id = 1 USING OPERATION_ID 'd2'"),
        "23503"
    );
}

#[test]
fn set_operations_join_and_upsert_mix_text_and_varchar() {
    let (_p, _g, core) = open("vc-mix");
    setup_pair(&core);
    for op in ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"] {
        let mixed = rows(
            &core,
            &format!("SELECT k FROM tt {op} SELECT k FROM vt LIMIT 20"),
        );
        let same = rows(
            &core,
            &format!("SELECT k FROM tt {op} SELECT k FROM tt LIMIT 20"),
        );
        assert_eq!(mixed.len(), same.len(), "{op}");
    }
    assert_eq!(
        rows(&core, "SELECT tt.k FROM tt JOIN vt ON tt.k = vt.k LIMIT 10").len(),
        3
    );
    ok(
        &core,
        "INSERT INTO vt (id, k, v) VALUES (1, 'apple', 'x') \
         ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v USING OPERATION_ID 'up1'",
    );
}

#[test]
fn alter_column_type_converts_between_text_and_varchar() {
    let (path, _g, core) = open("vc-alter");
    ok(
        &core,
        "CREATE TABLE t (k TEXT PRIMARY KEY, u TEXT, UNIQUE (u), w TEXT)",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, u, w) VALUES (1, 'a', 'x', 'p') USING OPERATION_ID 'i1'",
    );
    let before = rows(&core, "SELECT k, u, w FROM t LIMIT 10");
    ok(&core, "ALTER TABLE t ALTER COLUMN k TYPE VARCHAR");
    ok(&core, "ALTER TABLE t ALTER COLUMN u TYPE CHARACTER VARYING");
    ok(&core, "ALTER TABLE t ALTER COLUMN w TYPE VARCHAR");
    assert_eq!(rows(&core, "SELECT k, u, w FROM t LIMIT 10"), before);
    // 変換後も PK／UNIQUE 違反を検出する。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k, u) VALUES (2, 'a', 'q') USING OPERATION_ID 'i2'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k, u) VALUES (2, 'b', 'x') USING OPERATION_ID 'i3'"
        ),
        "23505"
    );
    // 同一型への変更は従来どおり拒否する。
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN w TYPE VARCHAR"),
        "42804"
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN k TYPE TEXT");
    ok(&core, "ALTER TABLE t ALTER COLUMN w TYPE TEXT");
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k, u) VALUES (2, 'a', 'q') USING OPERATION_ID 'i4'"
        ),
        "23505"
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "k"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "u"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "w"), ColumnType::Text);
}

/// CHECK が参照する列の TEXT↔VARCHAR 変換は、既存ゲート（行を書き換えない変換は
/// `DependentObjectsStillExist`）のまま拒否する（Issue #2305 の保守的判断）。
#[test]
fn alter_column_type_on_check_dependent_column_stays_rejected() {
    let (_p, _g, core) = open("vc-alter-check");
    ok(
        &core,
        "CREATE TABLE t (c TEXT CONSTRAINT c_ne CHECK (c = 'ok'))",
    );
    assert!(exec(&core, "ALTER TABLE t ALTER COLUMN c TYPE VARCHAR").is_err());
}
