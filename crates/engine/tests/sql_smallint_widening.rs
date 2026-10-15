//! `SMALLINT` の拡大変換・縮小・FK・算術結果型・集合演算・JOIN（TABLE-30・TABLE-19・TABLE-40・
//! Issue #1560 の結合テスト。ポインタ表記のみ）。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) `ALTER COLUMN TYPE` の
//! 拡大（SMALLINT→INTEGER→BIGINT・SMALLINT→BIGINT）が値・型・索引を保ち再オープン後も同じ
//! こと、(2) SMALLINT への縮小は範囲外が 1 行でもあれば（他テナントの Private 行を含め）`22003` で
//! 失敗し表が一切変わらないこと、(3) 整数型混在の FK・集合演算・JOIN、(4) 算術の結果型を固定する。
//! `BIGINT→INTEGER` などの一般の縮小は TABLE-40 の担当で `42804` のまま。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
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

fn reopen(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("reopen storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    ok_as(core, "alice", sql);
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) {
    exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec_as(core, "alice", sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn query_as(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

/// 先頭列を `i64`（NULL は `None`）として昇順に返す。
fn firsts_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Option<i64>> {
    let mut v: Vec<Option<i64>> = query_as(core, tenant, sql)
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::SignedInteger(v) => Some(v),
            Cell::Integer(v) => Some(i64::try_from(v).expect("fits")),
            Cell::Null => None,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn firsts(core: &EngineCore, sql: &str) -> Vec<Option<i64>> {
    firsts_as(core, "alice", sql)
}

fn first_type(r: &QueryResult) -> Option<ColumnType> {
    match r.columns.first().expect("column") {
        ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        other => panic!("unexpected column meta {other:?}"),
    }
}

fn column_type(core: &EngineCore, table: &str, col: &str) -> ColumnType {
    let r = query_as(core, "alice", &format!("SELECT {col} FROM {table} LIMIT 1"));
    first_type(&r).expect("static type")
}

fn catalog_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
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

fn insert(core: &EngineCore, table: &str, id: u64, cols: &str, vals: &str) {
    ok(
        core,
        &format!(
            "INSERT INTO {table} (id, {cols}) VALUES ({id}, {vals}) USING OPERATION_ID '{table}{id}'"
        ),
    );
}

/// 他テナント（tenant-b）の Private 行を持つ 1 列テーブルを SQL を介さず作る
/// （SQL の INSERT は可視性を指定できないため）。`wide` は INTEGER／BIGINT 列の値。
fn open_with_foreign_private_row(
    label: &str,
    table: &str,
    ty: ColumnType,
    wide: i64,
) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            table,
            vec![ColumnDef::new("s", ty.clone(), true)],
        ))
        .expect("create table");
    let value = if ty == ColumnType::BigInt {
        Value::BigInt(wide)
    } else {
        Value::Integer(i32::try_from(wide).expect("fits i32"))
    };
    engine::tenant::insert_typed_row(
        &storage,
        table,
        &ctx("tenant-b"),
        2,
        Visibility::Private,
        &[value],
        &OperationId::parse("seed-tenant-b-2").expect("valid operation id"),
    )
    .expect("insert foreign private row");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

const POOL: [i64; 4] = [-32768, -1, 0, 32767];

fn seed_t(core: &EngineCore) {
    ok(core, "CREATE TABLE t (n SMALLINT)");
    for (i, v) in POOL.iter().enumerate() {
        insert(core, "t", i as u64 + 1, "n", &v.to_string());
    }
    ok(
        core,
        "INSERT INTO t (id) VALUES (9) USING OPERATION_ID 't9'",
    );
}

fn pool_with_null() -> Vec<Option<i64>> {
    let mut v: Vec<Option<i64>> = POOL.iter().map(|x| Some(*x)).collect();
    v.push(None);
    v.sort_unstable();
    v
}

/// AC1: SMALLINT→INTEGER→BIGINT。境界値・NULL・型が保たれ再オープン後も同じ。
#[test]
fn widening_chain_preserves_values_and_survives_reopen() {
    let (path, _g, core) = open("sw-chain");
    seed_t(&core);
    ok(&core, "ALTER TABLE t ALTER COLUMN n TYPE INTEGER");
    assert_eq!(column_type(&core, "t", "n"), ColumnType::Integer);
    assert_eq!(firsts(&core, "SELECT n FROM t LIMIT 10"), pool_with_null());
    ok(&core, "ALTER TABLE t ALTER COLUMN n TYPE BIGINT");
    assert_eq!(column_type(&core, "t", "n"), ColumnType::BigInt);
    assert_eq!(firsts(&core, "SELECT n FROM t LIMIT 10"), pool_with_null());
    // BIGINT になった列は i16 の外も保持できる。
    insert(&core, "t", 10, "n", "100000");
    drop(core);
    assert_eq!(catalog_type(&path, "t", "n"), ColumnType::BigInt);
    let core = reopen(&path);
    let mut want = pool_with_null();
    want.push(Some(100000));
    want.sort_unstable();
    assert_eq!(firsts(&core, "SELECT n FROM t LIMIT 10"), want);
}

/// AC1: SMALLINT→BIGINT の直接変換。
#[test]
fn smallint_to_bigint_directly_preserves_values() {
    let (path, _g, core) = open("sw-direct");
    seed_t(&core);
    ok(&core, "ALTER TABLE t ALTER COLUMN n TYPE BIGINT");
    drop(core);
    assert_eq!(catalog_type(&path, "t", "n"), ColumnType::BigInt);
    let core = reopen(&path);
    assert_eq!(firsts(&core, "SELECT n FROM t LIMIT 10"), pool_with_null());
}

/// PK／UNIQUE／索引付きの SMALLINT 列の拡大後も一意性違反（`23505`）と索引検索が働く。
#[test]
fn widening_key_columns_keeps_uniqueness_and_index_lookups() {
    for target in ["INTEGER", "BIGINT"] {
        let (_p, _g, core) = open(&format!("sw-key-{target}"));
        ok(
            &core,
            "CREATE TABLE k (a SMALLINT PRIMARY KEY, u SMALLINT UNIQUE, s SMALLINT)",
        );
        ok(&core, "CREATE INDEX k_s ON k (s)");
        for i in 1..=3 {
            insert(&core, "k", i, "a, u, s", &format!("{i}, {i}0, {i}00"));
        }
        for col in ["a", "u", "s"] {
            ok(
                &core,
                &format!("ALTER TABLE k ALTER COLUMN {col} TYPE {target}"),
            );
        }
        assert_eq!(
            code(
                &core,
                "INSERT INTO k (id, a, u, s) VALUES (4, 1, 99, 1) USING OPERATION_ID 'd1'"
            ),
            "23505"
        );
        assert_eq!(
            code(
                &core,
                "INSERT INTO k (id, a, u, s) VALUES (4, 9, 10, 1) USING OPERATION_ID 'd2'"
            ),
            "23505"
        );
        assert_eq!(
            firsts(&core, "SELECT id FROM k WHERE s = 200 LIMIT 10"),
            vec![Some(2)]
        );
    }
}

/// 縮小 INTEGER→SMALLINT・BIGINT→SMALLINT の成功。値・索引・一意性が保たれる。
#[test]
fn narrowing_to_smallint_succeeds_when_in_range() {
    for src in ["INTEGER", "BIGINT"] {
        let (path, _g, core) = open(&format!("sw-narrow-{src}"));
        ok(
            &core,
            &format!("CREATE TABLE k (a {src} PRIMARY KEY, s {src})"),
        );
        ok(&core, "CREATE INDEX k_s ON k (s)");
        for (i, v) in POOL.iter().enumerate() {
            insert(&core, "k", i as u64 + 1, "a, s", &format!("{v}, {v}"));
        }
        ok(&core, "ALTER TABLE k ALTER COLUMN a TYPE SMALLINT");
        ok(&core, "ALTER TABLE k ALTER COLUMN s TYPE SMALLINT");
        assert_eq!(column_type(&core, "k", "a"), ColumnType::SmallInt);
        assert_eq!(
            firsts(&core, "SELECT s FROM k LIMIT 10"),
            POOL.iter().map(|x| Some(*x)).collect::<Vec<_>>()
        );
        assert_eq!(
            code(
                &core,
                "INSERT INTO k (id, a, s) VALUES (9, 0, 1) USING OPERATION_ID 'dup'"
            ),
            "23505"
        );
        assert_eq!(
            code(
                &core,
                "INSERT INTO k (id, a, s) VALUES (9, 40000, 1) USING OPERATION_ID 'big'"
            ),
            "22003"
        );
        assert_eq!(
            firsts(&core, "SELECT id FROM k WHERE s = 32767 LIMIT 10"),
            vec![Some(4)]
        );
        drop(core);
        assert_eq!(catalog_type(&path, "k", "a"), ColumnType::SmallInt);
    }
}

/// AC2: 範囲外の行が 1 行でもあれば `22003`。範囲外は他テナントの Private 行にだけ置き、
/// 全テナント・全可視性を検証していること、失敗後に表が一切変わらないこと、エラー文言に値・
/// テナントが含まれないことを確認する。
#[test]
fn narrowing_fails_closed_across_tenants_and_leaves_table_unchanged() {
    for src in ["INTEGER", "BIGINT"] {
        let ty = if src == "INTEGER" {
            ColumnType::Integer
        } else {
            ColumnType::BigInt
        };
        // 範囲外は tenant-b の Private 行（alice からは不可視）。
        let (path, _g, core) =
            open_with_foreign_private_row(&format!("sw-fail-{src}"), "k", ty, 40000);
        ok(&core, "CREATE INDEX k_s ON k (s)");
        insert(&core, "k", 1, "s", "5");
        let e = exec_as(&core, "alice", "ALTER TABLE k ALTER COLUMN s TYPE SMALLINT")
            .expect_err("out of range");
        assert_eq!(e.wire_code(), "22003");
        let msg = format!("{e:?}{e}");
        assert!(!msg.contains("40000") && !msg.contains("tenant-b"), "{msg}");
        // 型・行・索引検索は変わらない。
        let want = if src == "INTEGER" {
            ColumnType::Integer
        } else {
            ColumnType::BigInt
        };
        assert_eq!(column_type(&core, "k", "s"), want);
        assert_eq!(firsts(&core, "SELECT s FROM k LIMIT 10"), vec![Some(5)]);
        assert_eq!(
            firsts_as(&core, "tenant-b", "SELECT s FROM k LIMIT 10"),
            vec![Some(40000)]
        );
        assert_eq!(
            firsts(&core, "SELECT id FROM k WHERE s = 5 LIMIT 10"),
            vec![Some(1)]
        );
        drop(core);
        assert_eq!(catalog_type(&path, "k", "s"), want);
    }
}

/// 縮小時は列 DEFAULT も範囲内であること（`ADD COLUMN ... NOT NULL DEFAULT` は既存行に
/// バイトが無く、行の走査だけでは見逃す）。
#[test]
fn narrowing_checks_column_default_range() {
    let (_p, _g, core) = open("sw-default");
    ok(&core, "CREATE TABLE d (x INTEGER)");
    insert(&core, "d", 1, "x", "1");
    ok(
        &core,
        "ALTER TABLE d ADD COLUMN y INTEGER NOT NULL DEFAULT 40000",
    );
    ok(
        &core,
        "ALTER TABLE d ADD COLUMN z INTEGER NOT NULL DEFAULT 7",
    );
    assert_eq!(
        code(&core, "ALTER TABLE d ALTER COLUMN y TYPE SMALLINT"),
        "22003"
    );
    assert_eq!(column_type(&core, "d", "y"), ColumnType::Integer);
    ok(&core, "ALTER TABLE d ALTER COLUMN z TYPE SMALLINT");
    assert_eq!(firsts(&core, "SELECT z FROM d LIMIT 10"), vec![Some(7)]);
}

/// スコープ境界: `BIGINT→INTEGER` などの一般の縮小は従来どおり `42804`。
#[test]
fn general_narrowing_stays_rejected() {
    let (_p, _g, core) = open("sw-general");
    ok(&core, "CREATE TABLE g (b BIGINT, i INTEGER, s SMALLINT)");
    assert_eq!(
        code(&core, "ALTER TABLE g ALTER COLUMN b TYPE INTEGER"),
        "42804"
    );
    assert_eq!(
        code(&core, "ALTER TABLE g ALTER COLUMN s TYPE REAL"),
        "42804"
    );
    assert_eq!(
        code(&core, "ALTER TABLE g ALTER COLUMN i TYPE REAL"),
        "42804"
    );
}

fn setup_fk(core: &EngineCore, parent_ty: &str, child_ty: &str, extra: &str) {
    ok(
        core,
        &format!("CREATE TABLE kp (k {parent_ty} PRIMARY KEY, u {parent_ty} UNIQUE)"),
    );
    ok(
        core,
        &format!("CREATE TABLE kc (r {child_ty}, FOREIGN KEY (r) REFERENCES kp (k){extra})"),
    );
    for k in [1, 2, 3] {
        insert(core, "kp", k, "k, u", &format!("{k}, {k}0"));
    }
}

/// FK の整数型混在（全 9 組）。違反は `23503`、親の削除も保護される。
#[test]
fn foreign_keys_accept_mixed_integer_types() {
    let tys = ["SMALLINT", "INTEGER", "BIGINT"];
    for (pi, p) in tys.iter().enumerate() {
        for (ci, c) in tys.iter().enumerate() {
            let (_path, _g, core) = open(&format!("sw-fk-{pi}{ci}"));
            setup_fk(&core, p, c, "");
            insert(&core, "kc", 1, "r", "2");
            assert_eq!(
                code(
                    &core,
                    "INSERT INTO kc (id, r) VALUES (2, 99) USING OPERATION_ID 'bad'"
                ),
                "23503",
                "{c} -> {p}"
            );
            assert_eq!(
                code(
                    &core,
                    "DELETE FROM kp WHERE id = 2 USING OPERATION_ID 'del'"
                ),
                "23503",
                "{c} -> {p}"
            );
            ok(
                &core,
                "DELETE FROM kp WHERE id = 3 USING OPERATION_ID 'del3'",
            );
        }
    }
}

/// 子が INTEGER／BIGINT で親が SMALLINT のとき、親の値域外の値を持つ子行は挿入できない。
#[test]
fn wide_child_value_cannot_reference_smallint_parent() {
    for c in ["INTEGER", "BIGINT"] {
        let (_p, _g, core) = open(&format!("sw-fk-wide-{c}"));
        setup_fk(&core, "SMALLINT", c, "");
        assert_eq!(
            code(
                &core,
                "INSERT INTO kc (id, r) VALUES (1, 100000) USING OPERATION_ID 'wide'"
            ),
            "23503"
        );
    }
}

/// SMALLINT の子が `id` を参照する FK と、既存行を検証する ADD FOREIGN KEY。
#[test]
fn foreign_key_smallint_child_references_id_and_add_foreign_key() {
    let (_p, _g, core) = open("sw-fk-id");
    ok(&core, "CREATE TABLE ap (v INTEGER)");
    insert(&core, "ap", 1, "v", "1");
    ok(&core, "CREATE TABLE ac (r SMALLINT REFERENCES ap)");
    insert(&core, "ac", 1, "r", "1");
    assert_eq!(
        code(
            &core,
            "INSERT INTO ac (id, r) VALUES (2, 77) USING OPERATION_ID 'bad'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "DELETE FROM ap WHERE id = 1 USING OPERATION_ID 'del'"
        ),
        "23503"
    );
    ok(&core, "CREATE TABLE bp (k SMALLINT PRIMARY KEY)");
    insert(&core, "bp", 1, "k", "5");
    ok(&core, "CREATE TABLE bc (r INTEGER)");
    insert(&core, "bc", 1, "r", "5");
    ok(
        &core,
        "ALTER TABLE bc ADD FOREIGN KEY (r) REFERENCES bp (k)",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO bc (id, r) VALUES (2, 6) USING OPERATION_ID 'bad2'"
        ),
        "23503"
    );
}

/// ON UPDATE CASCADE で親の値が子の SMALLINT に収まらない場合は fail-closed で副作用ゼロ。
#[test]
fn on_update_cascade_out_of_smallint_range_is_rejected_without_side_effects() {
    let (_p, _g, core) = open("sw-fk-cascade");
    setup_fk(&core, "INTEGER", "SMALLINT", " ON UPDATE CASCADE");
    insert(&core, "kc", 1, "r", "1");
    assert_eq!(
        code(
            &core,
            "UPDATE kp SET k = 40000 WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "23503"
    );
    assert_eq!(
        firsts(&core, "SELECT k FROM kp LIMIT 10"),
        vec![Some(1), Some(2), Some(3)]
    );
    assert_eq!(firsts(&core, "SELECT r FROM kc LIMIT 10"), vec![Some(1)]);
    ok(
        &core,
        "UPDATE kp SET k = 100 WHERE id = 1 USING OPERATION_ID 'u2'",
    );
    assert_eq!(firsts(&core, "SELECT r FROM kc LIMIT 10"), vec![Some(100)]);
}

/// FK の構成列を ALTER（拡大・縮小）した後も参照整合性が働く。
#[test]
fn foreign_key_columns_altered_keep_referential_integrity() {
    let (_p, _g, core) = open("sw-fk-alter");
    setup_fk(&core, "SMALLINT", "SMALLINT", "");
    insert(&core, "kc", 1, "r", "1");
    ok(&core, "ALTER TABLE kp ALTER COLUMN k TYPE BIGINT");
    assert_eq!(
        code(
            &core,
            "INSERT INTO kc (id, r) VALUES (2, 99) USING OPERATION_ID 'bad'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "DELETE FROM kp WHERE id = 1 USING OPERATION_ID 'del'"
        ),
        "23503"
    );
    ok(&core, "ALTER TABLE kc ALTER COLUMN r TYPE INTEGER");
    ok(&core, "ALTER TABLE kp ALTER COLUMN k TYPE SMALLINT");
    insert(&core, "kc", 3, "r", "3");
    assert_eq!(
        code(
            &core,
            "INSERT INTO kc (id, r) VALUES (4, 99) USING OPERATION_ID 'bad2'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "DELETE FROM kp WHERE id = 3 USING OPERATION_ID 'del3'"
        ),
        "23503"
    );
}

/// CHECK 付きの列の SMALLINT↔INTEGER は受理し、CHECK が引き続き働く。
#[test]
fn check_constraint_columns_survive_integer_conversions() {
    let (_p, _g, core) = open("sw-check");
    ok(&core, "CREATE TABLE c (n SMALLINT, CHECK (n > 0))");
    insert(&core, "c", 1, "n", "5");
    ok(&core, "ALTER TABLE c ALTER COLUMN n TYPE INTEGER");
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, n) VALUES (2, -1) USING OPERATION_ID 'c2'"
        ),
        "23514"
    );
    ok(&core, "ALTER TABLE c ALTER COLUMN n TYPE SMALLINT");
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, n) VALUES (3, -1) USING OPERATION_ID 'c3'"
        ),
        "23514"
    );
    ok(&core, "ALTER TABLE c ALTER COLUMN n TYPE BIGINT");
    insert(&core, "c", 4, "n", "40000");
    assert_eq!(
        code(&core, "ALTER TABLE c ALTER COLUMN n TYPE SMALLINT"),
        "22003"
    );
}

/// 算術の結果型: smallint 同士は smallint、integer 混在は integer、bigint 混在は bigint。
#[test]
fn arithmetic_result_types_follow_postgres() {
    let (_p, _g, core) = open("sw-arith");
    ok(&core, "CREATE TABLE a (n SMALLINT, m INTEGER, b BIGINT)");
    insert(&core, "a", 1, "n, m, b", "200, 3, 4");
    let ty = |sql: &str| first_type(&query_as(&core, "alice", sql));
    assert_eq!(
        ty("SELECT n + n FROM a LIMIT 1"),
        Some(ColumnType::SmallInt)
    );
    assert_eq!(ty("SELECT n + m FROM a LIMIT 1"), Some(ColumnType::Integer));
    assert_eq!(ty("SELECT n + b FROM a LIMIT 1"), Some(ColumnType::BigInt));
    assert_eq!(ty("SELECT m + b FROM a LIMIT 1"), Some(ColumnType::BigInt));
    assert_eq!(
        firsts(&core, "SELECT n * 100 FROM a LIMIT 1"),
        vec![Some(20000)]
    );
    assert_eq!(code(&core, "SELECT n * n FROM a LIMIT 1"), "22003");
    assert_eq!(
        firsts(&core, "SELECT n * m FROM a LIMIT 1"),
        vec![Some(600)]
    );
}

/// 集合演算: 整数型の混在は結果列が広い方の型になり、重複除去が型をまたいで働く。
#[test]
fn set_operations_unify_integer_types() {
    let (_p, _g, core) = open("sw-setop");
    ok(&core, "CREATE TABLE s1 (v SMALLINT)");
    ok(&core, "CREATE TABLE s2 (v INTEGER)");
    ok(&core, "CREATE TABLE s3 (v BIGINT)");
    for (i, v) in [1, 2, 3].iter().enumerate() {
        insert(&core, "s1", i as u64 + 1, "v", &v.to_string());
    }
    for (i, v) in [2, 3, 100000].iter().enumerate() {
        insert(&core, "s2", i as u64 + 1, "v", &v.to_string());
    }
    for (i, v) in [3, 5_000_000_000i64].iter().enumerate() {
        insert(&core, "s3", i as u64 + 1, "v", &v.to_string());
    }
    let r = query_as(&core, "alice", "SELECT v FROM s1 UNION SELECT v FROM s2");
    assert_eq!(first_type(&r), Some(ColumnType::Integer));
    assert_eq!(
        firsts(&core, "SELECT v FROM s1 UNION SELECT v FROM s2"),
        vec![Some(1), Some(2), Some(3), Some(100000)]
    );
    assert_eq!(
        firsts(&core, "SELECT v FROM s1 UNION ALL SELECT v FROM s2"),
        vec![Some(1), Some(2), Some(2), Some(3), Some(3), Some(100000)]
    );
    assert_eq!(
        firsts(&core, "SELECT v FROM s1 INTERSECT SELECT v FROM s2"),
        vec![Some(2), Some(3)]
    );
    assert_eq!(
        firsts(&core, "SELECT v FROM s1 EXCEPT SELECT v FROM s3"),
        vec![Some(1), Some(2)]
    );
    let r = query_as(&core, "alice", "SELECT v FROM s3 UNION SELECT v FROM s1");
    assert_eq!(first_type(&r), Some(ColumnType::BigInt));
    // 整数型以外との混在は従来どおり 42804。
    ok(&core, "CREATE TABLE s4 (v TEXT)");
    assert_eq!(
        code(&core, "SELECT v FROM s1 UNION SELECT v FROM s4"),
        "42804"
    );
}

/// JOIN: smallint 列 = integer 列、smallint 列 = 疑似列 id。
#[test]
fn join_keys_accept_smallint() {
    let (_p, _g, core) = open("sw-join");
    ok(&core, "CREATE TABLE ja (k SMALLINT)");
    ok(&core, "CREATE TABLE jb (k INTEGER, tag INTEGER)");
    for i in 1..=3u64 {
        insert(&core, "ja", i, "k", &i.to_string());
    }
    insert(&core, "jb", 1, "k, tag", "2, 20");
    insert(&core, "jb", 2, "k, tag", "3, 30");
    insert(&core, "jb", 3, "k, tag", "100000, 40");
    assert_eq!(
        firsts(
            &core,
            "SELECT jb.tag FROM ja JOIN jb ON ja.k = jb.k LIMIT 10"
        ),
        vec![Some(20), Some(30)]
    );
    assert_eq!(
        firsts(
            &core,
            "SELECT jb.tag FROM ja JOIN jb ON ja.k = jb.id LIMIT 10"
        ),
        vec![Some(20), Some(30), Some(40)]
    );
}

/// RLS: 縮小の失敗は他テナントの行の値を結果・エラーに現さない。
#[test]
fn other_tenants_rows_never_appear_in_results_or_errors() {
    let (_p, _g, core) = open_with_foreign_private_row("sw-rls", "r", ColumnType::Integer, 70000);
    insert(&core, "r", 3, "s", "7");
    let e = exec_as(&core, "alice", "ALTER TABLE r ALTER COLUMN s TYPE SMALLINT")
        .expect_err("out of range");
    assert_eq!(e.wire_code(), "22003");
    assert!(!format!("{e:?}{e}").contains("70000"));
    assert_eq!(
        firsts_as(&core, "alice", "SELECT s FROM r LIMIT 10"),
        vec![Some(7)]
    );
}
