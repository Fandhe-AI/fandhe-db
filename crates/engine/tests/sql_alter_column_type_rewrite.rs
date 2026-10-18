//! `ALTER TABLE <t> ALTER COLUMN <c> TYPE <型> [USING <式>]` の全行書き換え（TABLE-40 (1)(2)(5)・
//! TASK-278、Issue #1636）の SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session` を production 経路として、(1) USING 省略の縮小・非互換変換
//! （代入変換がある組だけ受理）、(2) USING 式の評価と代入変換、(3) 1 行でも変換・評価に失敗すれば
//! 文全体を拒否して表が一切変わらないこと（違反行が呼び出し元から見えない他テナントの Private 行でも
//! 同じで、エラー文言に値・テナント・行 id が出ないこと）、(4) 依存を持つ列の暫定 `0A000`
//! （Issue #1637 まで）、(5) 永続性とキャッシュの失効を固定する。

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
    exec_as(core, "alice", sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn fail(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    exec_as(core, "alice", sql).expect_err(&format!("{sql} must fail"))
}

fn query_as(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

/// 先頭列を表示用の文字列（NULL は `NULL`）として昇順に返す。
fn firsts_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = query_as(core, tenant, sql)
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn firsts(core: &EngineCore, sql: &str) -> Vec<String> {
    firsts_as(core, "alice", sql)
}

fn column_type(core: &EngineCore, table: &str, col: &str) -> ColumnType {
    let r = query_as(core, "alice", &format!("SELECT {col} FROM {table} LIMIT 1"));
    match r.columns.first().expect("column") {
        ColumnMeta::Scalar { ty, .. } => ty.clone(),
        ColumnMeta::Computed { ty, .. } => ty.clone().expect("static type"),
        other => panic!("unexpected column meta {other:?}"),
    }
}

fn insert(core: &EngineCore, table: &str, id: u64, cols: &str, vals: &str) {
    ok(
        core,
        &format!(
            "INSERT INTO {table} (id, {cols}) VALUES ({id}, {vals}) USING OPERATION_ID '{table}{id}'"
        ),
    );
}

/// 1 列（`n`）の表に、`alice` の公開行を `count` 件（値は `id % 100`）と、最後に他テナント
/// `tenant-b` の Private 行（値 `last`）を SQL を介さず作る。違反行が呼び出し元から見えない
/// 位置（キー順の最後）に来る。
fn open_big_with_foreign_row(
    label: &str,
    ty: ColumnType,
    count: u64,
    last: Value,
) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "big",
            vec![ColumnDef::new("n", ty.clone(), true)],
        ))
        .expect("create table");
    for id in 1..=count {
        let small = i64::try_from(id % 100).expect("small");
        let value = match ty {
            ColumnType::BigInt => Value::BigInt(small),
            ColumnType::Text => Value::Text(small.to_string()),
            _ => panic!("unsupported seed type"),
        };
        engine::tenant::insert_typed_row(
            &storage,
            "big",
            &ctx("alice"),
            id,
            Visibility::Public,
            &[value],
            &OperationId::parse(&format!("seed-alice-{id}")).expect("operation id"),
        )
        .expect("insert alice row");
    }
    engine::tenant::insert_typed_row(
        &storage,
        "big",
        &ctx("tenant-b"),
        count + 1,
        Visibility::Private,
        &[last],
        &OperationId::parse("seed-tenant-b-last").expect("operation id"),
    )
    .expect("insert foreign row");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

/// USING 省略の縮小（BIGINT→INTEGER）。範囲内なら値を保って新しい型になり、再オープン後も同じ。
#[test]
fn narrowing_without_using_converts_all_rows_and_persists() {
    let (path, _g, core) = open("rw-narrow");
    ok(&core, "CREATE TABLE t (n BIGINT)");
    for (i, v) in [-2_147_483_648_i64, -1, 0, 2_147_483_647]
        .iter()
        .enumerate()
    {
        insert(&core, "t", i as u64 + 1, "n", &v.to_string());
    }
    insert(&core, "t", 9, "n", "NULL");
    let before = firsts(&core, "SELECT n FROM t LIMIT 10");
    ok(&core, "ALTER TABLE t ALTER COLUMN n TYPE INTEGER");
    assert_eq!(column_type(&core, "t", "n"), ColumnType::Integer);
    assert_eq!(
        firsts(&core, "SELECT n FROM t LIMIT 10").len(),
        before.len()
    );
    // 範囲検索・等価検索が変換直後に新しい値で正しく返る（索引キャッシュの失効）。
    assert_eq!(
        firsts(&core, "SELECT n FROM t WHERE n = 0 LIMIT 10").len(),
        1
    );
    assert_eq!(
        firsts(&core, "SELECT n FROM t WHERE n > -2 LIMIT 10").len(),
        3
    );
    drop(core);
    let core = reopen(&path);
    assert_eq!(
        firsts(&core, "SELECT n FROM t LIMIT 10").len(),
        before.len()
    );
    assert_eq!(
        firsts(&core, "SELECT n FROM t WHERE n = 2147483647 LIMIT 10").len(),
        1
    );
}

/// 1025 行以上で、違反行が他テナントの Private 行（呼び出し元から不可視）でも文全体が拒否され、
/// 表が一切変わらず、エラー文言に値・テナント・行 id が出ない。
#[test]
fn out_of_range_row_of_another_tenant_rejects_whole_statement() {
    let (path, _g, core) = open_big_with_foreign_row(
        "rw-foreign-violation",
        ColumnType::BigInt,
        1100,
        Value::BigInt(5_000_000_000),
    );
    let before = firsts(&core, "SELECT n FROM big LIMIT 5000");
    let before_b = firsts_as(&core, "tenant-b", "SELECT n FROM big LIMIT 5000");
    let err = fail(&core, "ALTER TABLE big ALTER COLUMN n TYPE INTEGER");
    assert_eq!(err.wire_code(), "22003");
    let msg = err.to_string();
    for leaked in ["5000000000", "tenant-b", "1101"] {
        assert!(!msg.contains(leaked), "leaked {leaked:?} in {msg:?}");
    }
    assert_eq!(column_type(&core, "big", "n"), ColumnType::BigInt);
    assert_eq!(firsts(&core, "SELECT n FROM big LIMIT 5000"), before);
    assert_eq!(
        firsts_as(&core, "tenant-b", "SELECT n FROM big LIMIT 5000"),
        before_b
    );
    drop(core);
    let core = reopen(&path);
    assert_eq!(firsts(&core, "SELECT n FROM big LIMIT 5000"), before);
    assert_eq!(
        firsts_as(&core, "tenant-b", "SELECT n FROM big LIMIT 5000"),
        before_b
    );
    assert_eq!(column_type(&core, "big", "n"), ColumnType::BigInt);
}

/// 成功時も全テナント・全可視性の行が書き換えられ、行のテナント・可視性は保たれる
/// （他テナントの Private 行が、変換後の値を自分のセッションから読める）。
#[test]
fn success_rewrites_rows_of_every_tenant_and_visibility() {
    let (path, _g, core) = open_big_with_foreign_row(
        "rw-foreign-success",
        ColumnType::BigInt,
        1100,
        Value::BigInt(1_234_567),
    );
    let alice_before = firsts(&core, "SELECT n FROM big LIMIT 5000");
    ok(&core, "ALTER TABLE big ALTER COLUMN n TYPE INTEGER");
    assert_eq!(column_type(&core, "big", "n"), ColumnType::Integer);
    assert_eq!(
        firsts(&core, "SELECT n FROM big LIMIT 5000").len(),
        alice_before.len()
    );
    // tenant-b の Private 行は、tenant-b のセッションからだけ読め、変換後の値が見える。
    let b = firsts_as(&core, "tenant-b", "SELECT n FROM big LIMIT 5000");
    assert!(b.iter().any(|v| v.contains("1234567")), "{b:?}");
    // alice からは tenant-b の行が見えないまま（可視性・テナントを保つ）。
    let a = firsts(&core, "SELECT n FROM big LIMIT 5000");
    assert!(!a.iter().any(|v| v.contains("1234567")), "{a:?}");
    drop(core);
    let core = reopen(&path);
    let b = firsts_as(&core, "tenant-b", "SELECT n FROM big LIMIT 5000");
    assert!(b.iter().any(|v| v.contains("1234567")), "{b:?}");
}

/// `VARCHAR(n)` への変換で、超過分が半角空白だけなら切り詰めて成功する（代入の意味論）。
#[test]
fn varchar_conversion_trims_excess_trailing_spaces() {
    let (_path, _g, core) = open("rw-trailing-spaces");
    ok(&core, "CREATE TABLE t (s TEXT)");
    insert(&core, "t", 1, "s", "'abc   '");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN s TYPE VARCHAR(3) USING s",
    );
    assert_eq!(column_type(&core, "t", "s"), ColumnType::Varchar(Some(3)));
    assert_eq!(firsts(&core, "SELECT s FROM t LIMIT 5").len(), 1);
    assert_eq!(
        firsts(&core, "SELECT s FROM t WHERE s = 'abc' LIMIT 5").len(),
        1
    );
}

/// TEXT→INTEGER。USING 省略は `42804`、`USING note::integer` は成功、形式不正の行があれば
/// `22P02` で表は不変（文言に値が出ない）。
#[test]
fn text_to_integer_requires_using_and_rejects_malformed_values() {
    let (_path, _g, core) = open("rw-text-int");
    ok(&core, "CREATE TABLE t (note TEXT)");
    insert(&core, "t", 1, "note", "'10'");
    insert(&core, "t", 2, "note", "'20'");
    assert_eq!(
        fail(&core, "ALTER TABLE t ALTER COLUMN note TYPE INTEGER").wire_code(),
        "42804"
    );
    insert(&core, "t", 3, "note", "'not-a-number-SECRET'");
    let err = fail(
        &core,
        "ALTER TABLE t ALTER COLUMN note TYPE INTEGER USING note::integer",
    );
    assert_eq!(err.wire_code(), "22P02");
    assert!(!err.to_string().contains("SECRET"), "{err}");
    assert_eq!(column_type(&core, "t", "note"), ColumnType::Text);
    assert_eq!(firsts(&core, "SELECT note FROM t LIMIT 10").len(), 3);

    ok(
        &core,
        "DELETE FROM t WHERE id = 3 USING OPERATION_ID 'del3'",
    );
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN note TYPE INTEGER USING note::integer",
    );
    assert_eq!(column_type(&core, "t", "note"), ColumnType::Integer);
    assert_eq!(
        firsts(&core, "SELECT note FROM t WHERE note > 15 LIMIT 10").len(),
        1
    );
}

/// USING が別の列を参照する式・CASE を含む式、`USING NULL`、NOT NULL 列への NULL 結果（`23502`）。
#[test]
fn using_expression_references_other_columns() {
    let (_path, _g, core) = open("rw-using-expr");
    ok(
        &core,
        "CREATE TABLE t (qty INTEGER, label TEXT, req INTEGER NOT NULL)",
    );
    insert(&core, "t", 1, "qty, label, req", "3, 'a', 1");
    insert(&core, "t", 2, "qty, label, req", "4, 'b', 2");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN qty TYPE BIGINT USING qty * 2",
    );
    assert_eq!(column_type(&core, "t", "qty"), ColumnType::BigInt);
    assert_eq!(
        firsts(&core, "SELECT qty FROM t WHERE qty = 8 LIMIT 10").len(),
        1
    );
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN label TYPE INTEGER USING CASE WHEN label = 'a' THEN 1 ELSE 2 END",
    );
    assert_eq!(
        firsts(&core, "SELECT label FROM t WHERE label = 2 LIMIT 10").len(),
        1
    );
    // nullable 列への `USING NULL` は成功する。
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN label TYPE TEXT USING NULL",
    );
    assert_eq!(
        firsts(&core, "SELECT label FROM t WHERE label IS NULL LIMIT 10").len(),
        2
    );
    // NOT NULL 列に NULL が入る変換は `23502` で表は不変。
    let err = fail(
        &core,
        "ALTER TABLE t ALTER COLUMN req TYPE BIGINT USING NULL",
    );
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(column_type(&core, "t", "req"), ColumnType::Integer);
}

/// 代入の意味論: `VARCHAR(n)` の超過は `22001`（切り詰めない）、NUMERIC は scale 縮小で丸め・
/// 精度超過は `22003`。
#[test]
fn assignment_semantics_for_length_and_precision() {
    let (_path, _g, core) = open("rw-assign");
    ok(&core, "CREATE TABLE t (s TEXT, d NUMERIC(8,3))");
    insert(&core, "t", 1, "s, d", "'abcdef', 1.235");
    let err = fail(
        &core,
        "ALTER TABLE t ALTER COLUMN s TYPE VARCHAR(3) USING s",
    );
    assert_eq!(err.wire_code(), "22001");
    assert!(!err.to_string().contains("abcdef"), "{err}");
    assert_eq!(column_type(&core, "t", "s"), ColumnType::Text);
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN s TYPE VARCHAR(6) USING s",
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE NUMERIC(8,1)");
    assert_eq!(
        firsts(&core, "SELECT d FROM t WHERE d = 1.2 LIMIT 10").len(),
        1
    );
    insert(&core, "t", 2, "s, d", "'x', 9999999.0");
    let err = fail(&core, "ALTER TABLE t ALTER COLUMN d TYPE NUMERIC(4,1)");
    assert_eq!(err.wire_code(), "22003");
}

/// 日時: DATE→TIMESTAMP が成功し、TIMESTAMP→TIMESTAMPTZ も受理される。
#[test]
fn datetime_conversions_succeed() {
    let (_path, _g, core) = open("rw-datetime");
    ok(&core, "CREATE TABLE t (d DATE, ts TIMESTAMP)");
    insert(
        &core,
        "t",
        1,
        "d, ts",
        "DATE '2024-03-01', TIMESTAMP '2024-03-01 12:00:00'",
    );
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE TIMESTAMP");
    assert_eq!(column_type(&core, "t", "d"), ColumnType::Timestamp);
    ok(&core, "ALTER TABLE t ALTER COLUMN ts TYPE TIMESTAMPTZ");
    assert_eq!(column_type(&core, "t", "ts"), ColumnType::TimestampTz);
    assert_eq!(firsts(&core, "SELECT d FROM t LIMIT 5").len(), 1);
    assert_eq!(firsts(&core, "SELECT ts FROM t LIMIT 5").len(), 1);
}

/// 依存を持つ列は、新しい一般変換では暫定的に `0A000`（索引・制約・ビューの再構築は Issue #1637）。
/// 副作用はなく、既存の受理経路（INTEGER→BIGINT）は依存があっても従来どおり成功する。
#[test]
fn dependent_columns_are_fail_closed_until_rebuild_support() {
    let (_path, _g, core) = open("rw-dependents");
    ok(
        &core,
        "CREATE TABLE p (k BIGINT PRIMARY KEY, u BIGINT UNIQUE, c BIGINT CHECK (c > 0), d BIGINT DEFAULT 5, ix BIGINT, vw BIGINT, n BIGINT)",
    );
    ok(&core, "CREATE TABLE ch (r BIGINT REFERENCES p (k))");
    ok(&core, "CREATE INDEX p_ix ON p (ix)");
    ok(&core, "CREATE VIEW pv AS SELECT vw FROM p");
    for col in ["k", "u", "c", "d", "ix", "vw"] {
        let err = fail(
            &core,
            &format!("ALTER TABLE p ALTER COLUMN {col} TYPE INTEGER"),
        );
        assert_eq!(err.wire_code(), "0A000", "{col}: {err}");
        assert_eq!(column_type(&core, "p", col), ColumnType::BigInt, "{col}");
    }
    // 子表の FK 参照元の列も同様。
    assert_eq!(
        fail(&core, "ALTER TABLE ch ALTER COLUMN r TYPE INTEGER").wire_code(),
        "0A000"
    );
    // 依存の無い列は成功する。
    ok(&core, "ALTER TABLE p ALTER COLUMN n TYPE INTEGER");
    assert_eq!(column_type(&core, "p", "n"), ColumnType::Integer);
}

/// `SELECT *` ビューは旧スキーマで全列を公開するため、どの列の一般変換も `0A000` で拒否する
/// （公開列型が変わる fail-open を避ける。表は不変）。
#[test]
fn select_star_view_blocks_general_conversion() {
    let (_path, _g, core) = open("rw-star-view");
    ok(&core, "CREATE TABLE t (n BIGINT)");
    ok(&core, "CREATE VIEW v AS SELECT * FROM t");
    let err = fail(&core, "ALTER TABLE t ALTER COLUMN n TYPE INTEGER");
    assert_eq!(err.wire_code(), "0A000", "{err}");
    assert_eq!(column_type(&core, "t", "n"), ColumnType::BigInt);
}

/// `USING <対象列>` は USING 省略と同じ扱いで、既存の拡大経路（カタログのみ）を通る。
#[test]
fn using_target_column_is_treated_as_omitted() {
    let (_path, _g, core) = open("rw-identity");
    ok(&core, "CREATE TABLE t (amount NUMERIC(5,2))");
    insert(&core, "t", 1, "amount", "1.50");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN amount TYPE NUMERIC(9,2) USING amount",
    );
    assert_eq!(
        column_type(&core, "t", "amount"),
        ColumnType::Numeric {
            precision: 9,
            scale: 2
        }
    );
    assert_eq!(
        firsts(&core, "SELECT amount FROM t WHERE amount = 1.5 LIMIT 10").len(),
        1
    );
}

/// TIMESTAMP→TIMESTAMPTZ はセッションの TimeZone を壁時計として解釈する（セッション TZ が
/// 変換の評価区間に張られていること）。同じ値を UTC と Asia/Tokyo で変換すると結果が異なる。
#[test]
fn timestamp_to_timestamptz_uses_session_time_zone() {
    let (_path, _g, core) = open("rw-tz");
    for (table, zone) in [("utc", "UTC"), ("tokyo", "Asia/Tokyo")] {
        ok(&core, &format!("CREATE TABLE {table} (ts TIMESTAMP)"));
        insert(&core, table, 1, "ts", "TIMESTAMP '2024-03-01 12:00:00'");
        let mut s = SessionState::default();
        s.allow_ddl();
        core.execute_sql_in_session(&ctx("alice"), &mut s, &format!("SET TIME ZONE '{zone}'"))
            .expect("set time zone");
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut s,
            &format!("ALTER TABLE {table} ALTER COLUMN ts TYPE TIMESTAMPTZ"),
        )
        .expect("alter type");
    }
    let utc = firsts(&core, "SELECT ts FROM utc LIMIT 1");
    let tokyo = firsts(&core, "SELECT ts FROM tokyo LIMIT 1");
    assert_ne!(utc, tokyo, "session time zone must affect the conversion");
}

/// 従来どおりの拒否: 複数アクション内・VECTOR・ENUM。
#[test]
fn unchanged_rejections() {
    let (_path, _g, core) = open("rw-rejections");
    ok(
        &core,
        "CREATE TABLE t (embedding VECTOR(2), qty INTEGER, note TEXT)",
    );
    ok(&core, "CREATE TYPE mood AS ENUM ('a', 'b')");
    assert_eq!(
        fail(
            &core,
            "ALTER TABLE t ALTER COLUMN qty TYPE BIGINT USING qty + 1, DROP COLUMN note"
        )
        .wire_code(),
        "0A000"
    );
    assert_eq!(
        fail(
            &core,
            "ALTER TABLE t ALTER COLUMN embedding TYPE TEXT USING embedding"
        )
        .wire_code(),
        "42804"
    );
    assert_eq!(
        fail(
            &core,
            "ALTER TABLE t ALTER COLUMN note TYPE mood USING note"
        )
        .wire_code(),
        "0A000"
    );
    assert_eq!(
        fail(&core, "ALTER TABLE t ALTER COLUMN note TYPE mood").wire_code(),
        "42804"
    );
}
