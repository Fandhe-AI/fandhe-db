//! セッション TimeZone（`SET TIME ZONE`／`SET timezone`／`SHOW timezone`。TABLE-33・Issue #1575）の
//! 結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`／`execute_sql_in_txn`）で、(1) 受理する
//! SET の各形と SHOW の表示値（期待値は PG 17 の実測）、(2) 不正値が固定文言の `22023` で拒否され
//! 失敗した SET がセッションを変えないこと、(3) TZ 無しの `TIMESTAMPTZ` 入力が現在のセッション
//! TZ で解釈され格納値（UTC）は後から SET しても変わらないこと（INSERT・UPDATE・UPSERT・COPY FROM）、
//! (4) トランザクションの取り消しで値が戻ること、(5) セッション間・テナント間で独立していること、
//! (6) 列 DEFAULT が DDL 時点のセッション TZ で固定されること、を固定する。
//! StartupMessage・ParameterStatus は wire-server 側の `wire_session_timezone` が担当する。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 結果行の先頭セル（`SELECT id ...` の宣言済み `id` 列）を `u64` で返す。採番方式の表は
/// `ResultRow::id` に内部 rowid を載せない（TABLE-23）ため、利用者向けの値はセルから読む。
fn row_id(r: &engine::sql::exec::ResultRow) -> u64 {
    match r.cells.first() {
        Some(engine::sql::exec::Cell::SignedInteger(n)) => u64::try_from(*n).expect("id >= 0"),
        Some(engine::sql::exec::Cell::Integer(n)) => *n,
        other => panic!("unexpected id cell: {other:?}"),
    }
}

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

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx_for("alice"), s, sql)
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) {
    run(core, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    run(core, s, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

/// `SHOW timezone` の 1 行目の text 値。
fn show(core: &EngineCore, s: &mut SessionState) -> String {
    match run(core, s, "SHOW timezone").expect("show") {
        SqlOutcome::Show(r) => {
            assert_eq!(r.columns.len(), 1);
            assert_eq!(r.rows.len(), 1);
            match &r.rows[0].cells[0] {
                Cell::Text(t) => t.clone(),
                other => panic!("unexpected cell {other:?}"),
            }
        }
        other => panic!("expected Show, got {other:?}"),
    }
}

/// `SELECT id, v` の UTC マイクロ秒（id 昇順ではなく id 指定で引く）。
fn stored(core: &EngineCore, id: u64) -> Option<i64> {
    let r = core
        .execute_sql(&ctx_for("alice"), "SELECT id, v FROM t LIMIT 50")
        .expect("select");
    r.rows
        .iter()
        .find(|row| row_id(row) == id)
        .and_then(|row| match row.cells.get(1) {
            Some(Cell::TimestampTz(m)) => Some(*m),
            _ => None,
        })
}

fn utc_text(m: i64) -> String {
    engine::datetime::format_timestamptz(m, &engine::datetime::SessionTimeZone::utc())
}

fn insert(core: &EngineCore, s: &mut SessionState, id: u64, lit: &str) {
    ok(
        core,
        s,
        &format!("INSERT INTO t (id, v) VALUES ({id}, '{lit}') USING OPERATION_ID 'op{id}'"),
    );
}

#[test]
fn set_forms_and_show_display_values() {
    let (_g, core) = open("sz-forms");
    let mut s = session();
    assert_eq!(show(&core, &mut s), "UTC");
    let cases: &[(&str, &str)] = &[
        ("SET TIME ZONE 'Asia/Tokyo'", "Asia/Tokyo"),
        ("SET TIME ZONE utc", "UTC"),
        ("SET SESSION TIME ZONE 'Japan'", "Japan"),
        ("SET timezone TO 'asia/tokyo'", "Asia/Tokyo"),
        ("SET \"TimeZone\" = 'UTC'", "UTC"),
        ("SET timezone = Japan", "Japan"),
        ("SET TIME ZONE 9", "<+09>-09"),
        ("SET TIME ZONE -8", "<-08>+08"),
        ("SET TIME ZONE 5.5", "<+05:30>-05:30"),
        ("SET TIME ZONE '+09:00'", "+09:00"),
        ("SET TIME ZONE 'est5edt'", "EST5EDT"),
        ("SET TIME ZONE 'UTC+9'", "UTC+9"),
        ("set time zone 'Asia/Tokyo';", "Asia/Tokyo"),
    ];
    for (sql, expected) in cases {
        assert_eq!(run(&core, &mut s, sql).expect(sql), SqlOutcome::SetTimeZone);
        assert_eq!(&show(&core, &mut s), expected, "{sql}");
    }
    for sql in ["SHOW TIME ZONE", "SHOW \"TimeZone\"", "show TimeZone;"] {
        match run(&core, &mut s, sql).expect(sql) {
            SqlOutcome::Show(r) => assert_eq!(r.rows.len(), 1, "{sql}"),
            other => panic!("expected Show, got {other:?}"),
        }
    }
}

#[test]
fn default_and_local_reset_to_the_reset_value() {
    let (_g, core) = open("sz-reset");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, "SET TIME ZONE DEFAULT");
    assert_eq!(show(&core, &mut s), "UTC");
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, "SET TIME ZONE LOCAL");
    assert_eq!(show(&core, &mut s), "UTC");
    ok(&core, &mut s, "SET timezone TO 'Asia/Tokyo'");
    ok(&core, &mut s, "SET timezone = DEFAULT");
    assert_eq!(show(&core, &mut s), "UTC");

    // StartupMessage の値が reset 値になる。
    s.apply_startup_time_zone(
        engine::datetime::SessionTimeZone::parse_setting("America/New_York").expect("tz"),
    );
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, "SET TIME ZONE DEFAULT");
    assert_eq!(show(&core, &mut s), "America/New_York");
    // 変数形の LOCAL は文字列 'local' と同じ扱いで 22023。
    assert_eq!(code(&core, &mut s, "SET timezone = LOCAL"), "22023");
}

#[test]
fn invalid_values_are_22023_with_fixed_message_and_keep_the_session() {
    let (_g, core) = open("sz-invalid");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    for sql in [
        "SET TIME ZONE 'JST'",
        "SET TIME ZONE 'Etc/Unknown'",
        "SET TIME ZONE 'Foo/Bar'",
        "SET TIME ZONE 'Z'",
        "SET TIME ZONE ''",
        "SET TIME ZONE 'local'",
        "SET TIME ZONE 'default'",
        "SET TIME ZONE '../etc'",
        "SET TIME ZONE 'Asia/Tokyo '",
        "SET TIME ZONE 168",
        "SET TIME ZONE 'nan'",
        "SET timezone = 'no_such_zone'",
    ] {
        let err = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "22023", "{sql}");
        let msg = err.client_message();
        assert_eq!(msg, "invalid value for parameter \"TimeZone\"", "{sql}");
        assert_eq!(show(&core, &mut s), "Asia/Tokyo", "{sql} must not change");
    }
}

#[test]
fn unsupported_forms_and_sessionless_entries() {
    let (_g, core) = open("sz-unsupported");
    let mut s = session();
    assert_eq!(code(&core, &mut s, "SET LOCAL TIME ZONE 'UTC'"), "0A000");
    assert_eq!(code(&core, &mut s, "SET LOCAL timezone = 'UTC'"), "0A000");
    assert_eq!(
        code(
            &core,
            &mut s,
            "SET TIME ZONE INTERVAL '+09:00' HOUR TO MINUTE"
        ),
        "0A000"
    );
    // 他の SET／SHOW の挙動は不変。
    assert_eq!(code(&core, &mut s, "SET other_var = 'x'"), "42601");
    assert_eq!(code(&core, &mut s, "SHOW ALL"), "42601");
    // セッションを持たない入口では SET／SHOW とも一律 42601。
    let c = ctx_for("alice");
    for sql in [
        "SET TIME ZONE 'UTC'",
        "SET TIME ZONE 'Nope/Nope'",
        "SHOW timezone",
    ] {
        assert_eq!(
            core.execute_sql(&c, sql).expect_err(sql).wire_code(),
            "42601",
            "{sql}"
        );
    }
}

#[test]
fn describe_reports_show_columns_and_set_no_data() {
    let (_g, core) = open("sz-describe");
    let s = session();
    let parsed = core.parse_sql("SHOW timezone").expect("parse");
    let cols = core
        .describe_parsed_in_session(&s, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(cols.len(), 1);
    let parsed = core.parse_sql("SET TIME ZONE 'UTC'").expect("parse");
    assert!(core
        .describe_parsed_in_session(&s, &parsed)
        .expect("describe")
        .is_none());
}

#[test]
fn naive_input_follows_the_session_zone_and_stored_value_is_fixed() {
    let (_g, core) = open("sz-interpret");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ, n INTEGER)",
    );
    insert(&core, &mut s, 1, "2026-01-01 09:00:00");
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    insert(&core, &mut s, 2, "2026-01-01 09:00:00");
    // オフセット付きは TZ に影響されない。
    insert(&core, &mut s, 3, "2026-01-01 09:00:00Z");
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    insert(&core, &mut s, 4, "2026-01-01 09:00:00");
    insert(&core, &mut s, 5, "2026-07-01 09:00:00");
    ok(&core, &mut s, "SET TIME ZONE 9");
    insert(&core, &mut s, 6, "2026-01-01 09:00:00");

    assert_eq!(
        utc_text(stored(&core, 1).unwrap()),
        "2026-01-01 09:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 2).unwrap()),
        "2026-01-01 00:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 3).unwrap()),
        "2026-01-01 09:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 4).unwrap()),
        "2026-01-01 14:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 5).unwrap()),
        "2026-07-01 13:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 6).unwrap()),
        "2026-01-01 00:00:00+00"
    );

    // 格納値は後から SET しても変わらない。
    let before = stored(&core, 2);
    ok(&core, &mut s, "SET TIME ZONE 'Europe/London'");
    assert_eq!(stored(&core, 2), before);
}

#[test]
fn update_and_upsert_follow_the_session_zone() {
    let (_g, core) = open("sz-update");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ, n INTEGER)",
    );
    insert(&core, &mut s, 1, "2026-01-01 00:00:00Z");
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(
        &core,
        &mut s,
        "UPDATE t SET v = '2026-01-02 09:00:00' WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        utc_text(stored(&core, 1).unwrap()),
        "2026-01-02 00:00:00+00"
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO t (id, v) VALUES (1, '2026-01-01 00:00:00Z') ON CONFLICT (id) \
         DO UPDATE SET v = '2026-01-03 09:00:00' USING OPERATION_ID 'x1'",
    );
    assert_eq!(
        utc_text(stored(&core, 1).unwrap()),
        "2026-01-03 00:00:00+00"
    );
}

#[test]
fn copy_from_uses_the_zone_at_copy_start() {
    let (_g, core) = open("sz-copy");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ)",
    );
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    let policy = ctx_for("alice");
    let plan = core
        .begin_copy(
            &policy,
            &s,
            "COPY t (id, v) FROM STDIN USING OPERATION_ID 'c1'",
        )
        .expect("begin");
    let mut cs = match plan {
        CopyPlan::From(c) => c,
        CopyPlan::To(..) => panic!("expected FROM"),
    };
    // COPY 中にセッションが変わっても開始時点の値で解釈する。
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    cs.feed(b"1\t2026-01-01 09:00:00\n2\t2026-01-01 09:00:00Z\n")
        .expect("feed");
    let batch = cs.finish().expect("finish");
    core.commit_copy_in(&policy, batch).expect("commit");
    assert_eq!(
        utc_text(stored(&core, 1).unwrap()),
        "2026-01-01 00:00:00+00"
    );
    assert_eq!(
        utc_text(stored(&core, 2).unwrap()),
        "2026-01-01 09:00:00+00"
    );
}

#[test]
fn rollback_restores_and_commit_keeps_the_value() {
    let (_g, core) = open("sz-txn");
    let c = ctx_for("alice");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    let mut step = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    step(&mut s, "BEGIN");
    step(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(s.time_zone().display(), "Asia/Tokyo");
    step(&mut s, "ROLLBACK");
    assert_eq!(s.time_zone().display(), "UTC");

    step(&mut s, "BEGIN");
    step(&mut s, "SET TIME ZONE 'Asia/Tokyo'");
    step(&mut s, "COMMIT");
    assert_eq!(s.time_zone().display(), "Asia/Tokyo");
}

#[test]
fn failed_transaction_rollback_restores_the_value() {
    let (_g, core) = open("sz-txn-fail");
    let c = ctx_for("alice");
    let mut s = session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SET TIME ZONE 'Asia/Tokyo'")
        .expect("set");
    // 存在しないテーブルへの文でトランザクションが失敗状態になる。
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT id FROM missing LIMIT 1")
        .expect_err("fails");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(s.time_zone().display(), "UTC");
}

#[test]
fn sessions_and_tenants_are_independent() {
    let (_g, core) = open("sz-isolation");
    let mut a = session();
    let mut b = session();
    ok(
        &core,
        &mut a,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ)",
    );
    ok(&core, &mut a, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(show(&core, &mut b), "UTC");
    // 別テナントの同名セッション値も独立。
    let mut other = SessionState::default();
    core.execute_sql_in_session(&ctx_for("bob"), &mut other, "SET TIME ZONE 'Europe/Paris'")
        .expect("set");
    assert_eq!(a.time_zone().display(), "Asia/Tokyo");
    assert_eq!(other.time_zone().display(), "Europe/Paris");
    // スコープの外（NoSQL 等）は UTC。
    assert_eq!(
        engine::datetime::current_session_time_zone().display(),
        "UTC"
    );
}

#[test]
fn column_default_is_interpreted_at_ddl_time() {
    let (_g, core) = open("sz-default");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ DEFAULT '2026-01-01 00:00:00', n INTEGER)",
    );
    ok(
        &core,
        &mut s,
        "ALTER TABLE t ADD COLUMN w TIMESTAMPTZ DEFAULT '2026-01-01 09:00:00'",
    );
    // 別セッション（UTC）で補完しても DDL 時点の解釈で固定される。
    let mut utc = session();
    ok(
        &core,
        &mut utc,
        "INSERT INTO t (id, n) VALUES (1, 5) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        utc_text(stored(&core, 1).unwrap()),
        "2025-12-31 15:00:00+00"
    );
    let r = core
        .execute_sql(&ctx_for("alice"), "SELECT id, w FROM t LIMIT 5")
        .expect("select");
    match r.rows[0].cells.get(1) {
        Some(Cell::TimestampTz(m)) => assert_eq!(utc_text(*m), "2026-01-01 00:00:00+00"),
        other => panic!("unexpected {other:?}"),
    }
    // 不正な既定値は従来どおり拒否される。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE u (id BIGINT PRIMARY KEY, v TIMESTAMPTZ DEFAULT 'nope')"
        ),
        "22007"
    );
}

#[test]
fn partitioned_resend_with_another_zone_does_not_rewrite() {
    let (_g, core) = open("sz-partitioned");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v TIMESTAMPTZ, n INTEGER)",
    );
    for i in 1..=3 {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO t (id, n) VALUES ({i}, {i}) USING OPERATION_ID 'seed{i}'"),
        );
    }
    let sql = "UPDATE t SET v = '2026-01-01 09:00:00' WHERE n >= 0 \
               USING OPERATION_ID 'p1' PARTITIONED CHUNK 2";
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, sql);
    // 同じ operation_id・同じ TZ の再送は重複（23505）。
    assert_eq!(code(&core, &mut s, sql), "23505");
    // 内容照合ハッシュは非 UTC の TimeZone を含むため、別の TZ での再送は内容不一致（22023）で
    // 拒否される。書き込まず、最初の解釈（Asia/Tokyo）の格納値が変わらない（fail-closed）。
    let first = stored(&core, 1);
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(code(&core, &mut s, sql), "22023");
    assert_eq!(stored(&core, 1), first);
    assert_eq!(utc_text(first.unwrap()), "2026-01-01 00:00:00+00");
}
