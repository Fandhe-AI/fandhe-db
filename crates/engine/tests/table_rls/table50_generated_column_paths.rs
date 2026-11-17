//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の、COPY FROM・BEFORE／AFTER ROW トリガ・ビュー・トリガ本体・
//! FK の参照アクションの各経路の結合テスト（Issue #3329。ポインタ: `docs/spec/04-behavior/` の TABLE-50・
//! RECOVER-11・TASK-345、設計記録: `docs/design/generated-column.md`）。
//!
//! 検証の軸は 4 つ。(1) BEFORE ROW トリガが基の列を書き換えた後に生成列が計算され、トリガが NEW の生成列へ
//! 代入しても計算結果で上書きされ、AFTER・RETURNING が計算後の値を見ること。(2) COPY の列リスト・列リスト省略・
//! 明示トランザクション・トリガ付きの表。(3) トリガ本体からの書き込みと FK の参照アクションの連鎖が生成列を
//! 計算すること。(4) 評価エラー・NOT NULL 違反で行・トリガの書いた行が残らないこと。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, InsertOutcome};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANT: &str = "tenant-a";
const OTHER: &str = "tenant-b";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    (guard, core)
}

/// テナントごとの接続相当（SCHEMA `s` を `search_path` の先頭にする。トリガは共有 `public` の表を
/// 対象にできないため、表は SCHEMA `s` へ作る）。
fn session(core: &EngineCore, tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    // 他テナントの接続では SCHEMA が無いことがあるため、作成の失敗（既存）は無視する。
    let _ = core.execute_sql_in_session(&ctx(tenant), &mut s, "CREATE SCHEMA s");
    core.execute_sql_in_session(&ctx(tenant), &mut s, "SET search_path TO s, public")
        .expect("set search_path");
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = session(core, tenant);
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn grid(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    let result = match ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        SqlOutcome::Returning(r) => r.result,
        other => panic!("{sql}: expected rows, got {other:?}"),
    };
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(show).collect())
        .collect()
}

fn g(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn func(core: &EngineCore, name: &str, body: &str) {
    ok(
        core,
        TENANT,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn mk_g(core: &EngineCore) {
    ok(
        core,
        TENANT,
        "CREATE TABLE s.g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, c INTEGER)",
    );
}

/// 共有 `public` の表を作る・読む（`search_path` 無しの接続）。トリガ本体は SCHEMA 修飾の内部キーの表へ
/// 書けないため、本体の書き込み先は共有 `public` の表にする。
fn ok_public(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()))
}

fn mk_audit(core: &EngineCore) {
    ok_public(core, TENANT, "CREATE TABLE audit (n INTEGER)");
}

/// COPY を wire-server と同じ入口（begin → feed → finish → commit）で確定する。
fn copy_in<'e>(
    core: &'e EngineCore,
    tenant: &str,
    s: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'e>,
    sql: &str,
    data: &[u8],
) -> Result<InsertOutcome, SqlSurfaceError> {
    let policy = ctx(tenant);
    let plan = core.begin_copy_in_txn(&policy, s, txn, sql)?;
    let CopyPlan::From(mut cs) = plan else {
        panic!("expected COPY FROM STDIN plan");
    };
    cs.feed(data)?;
    let batch = cs.finish()?;
    core.commit_copy_in_txn_in_session(&policy, s, txn, batch)
}

fn copy(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    data: &[u8],
) -> Result<InsertOutcome, SqlSurfaceError> {
    let mut s = session(core, tenant);
    let mut txn = core.new_session_transaction();
    copy_in(core, tenant, &mut s, &mut txn, sql, data)
}

/// BEFORE ROW INSERT が基の列を書き換え、NEW の生成列へ代入しても、生成列は書き換え後の値から計算される。
/// 複数行 INSERT・`RETURNING`・AFTER ROW（計算後の値を見る）も同じ。
#[test]
fn before_insert_trigger_runs_before_the_generated_computation() {
    let (_gd, core) = fixture("gen-p-bi");
    mk_g(&core);
    mk_audit(&core);
    func(
        &core,
        "bump",
        "BEGIN NEW.a := NEW.a + 10; NEW.b := 999; RETURN NEW; END",
    );
    func(
        &core,
        "after_row",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.b); RETURN NULL; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER tb BEFORE INSERT ON s.g FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER ta AFTER INSERT ON s.g FOR EACH ROW EXECUTE FUNCTION after_row()",
    );
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1)");
    assert_eq!(
        grid(
            &core,
            TENANT,
            "INSERT INTO g (a) VALUES (2), (3) RETURNING a, b"
        ),
        g(&[&["12", "24"], &["13", "26"]])
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[&["11", "22"], &["12", "24"], &["13", "26"]])
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT n FROM audit ORDER BY n"),
        g(&[&["22"], &["24"], &["26"]])
    );
    // 生成列への明示値は SQL 表層の束縛が拒否する。
    assert_eq!(
        code(&core, TENANT, "INSERT INTO g (a, b) VALUES (1, 5)"),
        "428C9"
    );
}

/// BEFORE ROW UPDATE が基の列を書き換えると、SET に無くても全生成列が再計算される。
#[test]
fn before_update_trigger_recomputes_all_generated_columns() {
    let (_gd, core) = fixture("gen-p-bu");
    mk_g(&core);
    ok(&core, TENANT, "INSERT INTO g (a, c) VALUES (1, 0)");
    func(
        &core,
        "bump",
        "BEGIN NEW.a := NEW.a + 1; NEW.b := 999; RETURN NEW; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER tb BEFORE UPDATE ON s.g FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    ok(&core, TENANT, "UPDATE g SET a = 5");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g"),
        g(&[&["6", "12"]])
    );
    // SET が生成列の入力を含まなくても、BEFORE が書き換えた入力から再計算される。
    ok(&core, TENANT, "UPDATE g SET c = 1");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b, c FROM g"),
        g(&[&["7", "14", "1"]])
    );
}

/// AFTER ROW だけのトリガ付きの表でも、SET の対象列に依存する生成列が再計算され、AFTER は計算後の
/// 値を見る。
#[test]
fn after_only_triggers_compute_on_insert_and_update() {
    let (_gd, core) = fixture("gen-p-after");
    mk_g(&core);
    mk_audit(&core);
    func(
        &core,
        "after_row",
        "BEGIN INSERT INTO audit (n) VALUES (NEW.b); RETURN NULL; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER ta AFTER INSERT OR UPDATE ON s.g FOR EACH ROW EXECUTE FUNCTION after_row()",
    );
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (4)");
    ok(&core, TENANT, "UPDATE g SET a = 7");
    assert_eq!(grid(&core, TENANT, "SELECT b FROM g"), g(&[&["14"]]));
    assert_eq!(
        grid(&core, TENANT, "SELECT n FROM audit ORDER BY n"),
        g(&[&["8"], &["14"]])
    );
}

/// BEFORE が `NULL` を返した行は書かれず、評価エラー（22012）と NOT NULL 違反（23502）は文全体を失敗させて
/// 行・トリガの書いた行を残さない。
#[test]
fn skipped_rows_and_failures_leave_no_trace() {
    let (_gd, core) = fixture("gen-p-fail");
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.d (a INTEGER, b INTEGER GENERATED ALWAYS AS (100 / a) STORED)",
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.n (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED NOT NULL)",
    );
    mk_audit(&core);
    func(
        &core,
        "skip_or_zero",
        "BEGIN INSERT INTO audit (n) VALUES (1); IF NEW.a = 1 THEN RETURN NULL; END IF; IF NEW.a = 2 THEN NEW.a := 0; END IF; RETURN NEW; END",
    );
    func(&core, "noop", "BEGIN RETURN NEW; END");
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t1 BEFORE INSERT ON s.d FOR EACH ROW EXECUTE FUNCTION skip_or_zero()",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t2 BEFORE INSERT ON s.n FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    // NULL 返却の行はスキップ（行は 0 件）。
    ok(&core, TENANT, "INSERT INTO d (a) VALUES (1)");
    assert!(grid(&core, TENANT, "SELECT a FROM d").is_empty());
    // 評価エラー: トリガが audit へ書いた行も残らない。
    let audit_before = grid(&core, TENANT, "SELECT n FROM audit").len();
    assert_eq!(code(&core, TENANT, "INSERT INTO d (a) VALUES (2)"), "22012");
    assert!(grid(&core, TENANT, "SELECT a FROM d").is_empty());
    assert_eq!(
        grid(&core, TENANT, "SELECT n FROM audit").len(),
        audit_before
    );
    // NOT NULL の生成列が NULL になる行は 23502（計算後に検査する）。
    ok(&core, TENANT, "INSERT INTO n (a) VALUES (3)");
    assert_eq!(
        code(&core, TENANT, "INSERT INTO n (a) VALUES (NULL)"),
        "23502"
    );
    assert_eq!(grid(&core, TENANT, "SELECT a, b FROM n"), g(&[&["3", "6"]]));
}

/// UPSERT: BEFORE INSERT の書き換え後に計算した値が `EXCLUDED` に見え、BEFORE UPDATE の後に再計算される。
#[test]
fn upsert_with_before_triggers_computes_generated_columns() {
    let (_gd, core) = fixture("gen-p-upsert");
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.u (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    func(&core, "bi", "BEGIN NEW.a := NEW.a + 1; RETURN NEW; END");
    func(&core, "bu", "BEGIN NEW.a := NEW.a + 100; RETURN NEW; END");
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t1 BEFORE INSERT ON s.u FOR EACH ROW EXECUTE FUNCTION bi()",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t2 BEFORE UPDATE ON s.u FOR EACH ROW EXECUTE FUNCTION bu()",
    );
    let sql = "INSERT INTO u (k, a) VALUES (1, 1) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a";
    ok(&core, TENANT, sql);
    assert_eq!(
        grid(&core, TENANT, "SELECT k, a, b FROM u"),
        g(&[&["1", "2", "4"]])
    );
    ok(&core, TENANT, sql);
    assert_eq!(
        grid(&core, TENANT, "SELECT k, a, b FROM u"),
        g(&[&["1", "102", "204"]])
    );
}

/// COPY: 列リスト省略は生成列を除いた並び、列リストの生成列は `428C9`（行を残さない）、NOT NULL の生成列は
/// 列リストの被覆を要求せず計算後に検査する。明示トランザクションの COPY も同じ。
#[test]
fn copy_computes_and_rejects_generated_columns_in_the_column_list() {
    let (_gd, core) = fixture("gen-p-copy");
    mk_g(&core);
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.n (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED NOT NULL)",
    );
    let out = copy(&core, TENANT, "COPY g FROM STDIN", b"1\t7\n2\t8\n").expect("copy");
    assert_eq!(out.rows_affected, 2);
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b, c FROM g ORDER BY a"),
        g(&[&["1", "2", "7"], &["2", "4", "8"]])
    );
    copy(
        &core,
        TENANT,
        "COPY g (a) FROM STDIN WITH (FORMAT csv, HEADER true)",
        b"a\n3\n",
    )
    .expect("csv copy");
    assert_eq!(
        grid(&core, TENANT, "SELECT b FROM g WHERE a = 3"),
        g(&[&["6"]])
    );
    // 列リストの生成列は CopyInResponse の前に 428C9。
    let err = copy(&core, TENANT, "COPY g (a, b) FROM STDIN", b"").expect_err("rejected");
    assert_eq!(err.wire_code(), "428C9");
    // NOT NULL の生成列は列リストに無くても通り、NULL になる行は 23502。
    copy(&core, TENANT, "COPY n (a) FROM STDIN", b"5\n").expect("not null generated");
    let err = copy(&core, TENANT, "COPY n (a) FROM STDIN", b"\\N\n").expect_err("null generated");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM n"),
        g(&[&["5", "10"]])
    );
    // 明示トランザクション: 計算され、COMMIT で見え、ROLLBACK で消える。
    let c = ctx(TENANT);
    let mut s = session(&core, TENANT);
    let mut txn = core.new_session_transaction();
    for (end, expected) in [("ROLLBACK", 0usize), ("COMMIT", 1usize)] {
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        copy_in(
            &core,
            TENANT,
            &mut s,
            &mut txn,
            "COPY g (a) FROM STDIN",
            b"9\n",
        )
        .expect("txn copy");
        core.execute_sql_in_txn(&c, &mut s, &mut txn, end)
            .expect("end");
        assert_eq!(
            grid(&core, TENANT, "SELECT b FROM g WHERE a = 9").len(),
            expected,
            "{end}"
        );
    }
    assert_eq!(
        grid(&core, TENANT, "SELECT b FROM g WHERE a = 9"),
        g(&[&["18"]])
    );
}

/// トリガ付きの表への COPY も BEFORE ROW の後に計算する。他テナントの行へは波及しない。
#[test]
fn copy_into_a_trigger_table_computes_after_before_row() {
    let (_gd, core) = fixture("gen-p-copy-trg");
    mk_g(&core);
    func(&core, "bump", "BEGIN NEW.a := NEW.a + 10; RETURN NEW; END");
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER tb BEFORE INSERT ON s.g FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    copy(&core, TENANT, "COPY g (a) FROM STDIN", b"1\n2\n").expect("copy");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[&["11", "22"], &["12", "24"]])
    );
    // 他テナントからは表も行も見えない。
    assert_eq!(code(&core, OTHER, "SELECT a FROM g"), "42P01");
}

/// ビュー経由の書き込み（INSERT・UPDATE）は基表の生成列を計算し、トリガ付きの基表でも同じ。
#[test]
fn view_writes_compute_even_when_the_base_table_has_triggers() {
    let (_gd, core) = fixture("gen-p-view");
    mk_g(&core);
    func(&core, "bump", "BEGIN NEW.a := NEW.a + 10; RETURN NEW; END");
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER tb BEFORE INSERT OR UPDATE ON s.g FOR EACH ROW EXECUTE FUNCTION bump()",
    );
    ok(&core, TENANT, "CREATE VIEW gv AS SELECT a, b, c FROM g");
    ok(&core, TENANT, "INSERT INTO gv (a) VALUES (1)");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g"),
        g(&[&["11", "22"]])
    );
    ok(&core, TENANT, "UPDATE gv SET c = 3");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b, c FROM g"),
        g(&[&["21", "42", "3"]])
    );
    assert_eq!(code(&core, TENANT, "UPDATE gv SET b = 1"), "428C9");
}

/// トリガ本体の INSERT・UPDATE も生成列を計算し、本体からの明示値は 428C9 で文全体を失敗させる
/// （本体の書き込み先は共有 `public` の表）。
#[test]
fn trigger_bodies_write_generated_tables_through_the_computation() {
    let (_gd, core) = fixture("gen-p-body");
    ok_public(
        &core,
        TENANT,
        "CREATE TABLE gp (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, TENANT, "CREATE TABLE s.src (a INTEGER)");
    func(
        &core,
        "to_g",
        "BEGIN INSERT INTO gp (a) VALUES (NEW.a); RETURN NEW; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t1 AFTER INSERT ON s.src FOR EACH ROW EXECUTE FUNCTION to_g()",
    );
    ok(&core, TENANT, "INSERT INTO src (a) VALUES (4)");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM gp"),
        g(&[&["4", "8"]])
    );
    func(
        &core,
        "upd_g",
        "BEGIN UPDATE gp SET a = NEW.a; RETURN NEW; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t2 AFTER UPDATE ON s.src FOR EACH ROW EXECUTE FUNCTION upd_g()",
    );
    ok(&core, TENANT, "UPDATE src SET a = 9");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM gp"),
        g(&[&["9", "18"]])
    );
    func(
        &core,
        "bad",
        "BEGIN INSERT INTO gp (a, b) VALUES (1, 1); RETURN OLD; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t3 AFTER DELETE ON s.src FOR EACH ROW EXECUTE FUNCTION bad()",
    );
    // 本体からの明示値は拒否され、元の DELETE も取り消される。
    let err = run(&core, TENANT, "DELETE FROM src").expect_err("explicit value in body");
    assert_eq!(err.wire_code(), "428C9", "{err}");
    assert_eq!(grid(&core, TENANT, "SELECT a FROM src").len(), 1);
    assert_eq!(grid(&core, TENANT, "SELECT a FROM gp").len(), 1);
}

/// ビュー経由の書き込みで基表のトリガ本体が別の生成列表へ書いても、本体の書き込みは計算される
/// （外側のビュー区間の基表照合に左右されない）。
#[test]
fn view_write_with_trigger_body_writing_another_generated_table() {
    let (_gd, core) = fixture("gen-p-view-body");
    ok_public(
        &core,
        TENANT,
        "CREATE TABLE gp (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, TENANT, "CREATE TABLE s.src (a INTEGER)");
    func(
        &core,
        "to_g",
        "BEGIN INSERT INTO gp (a) VALUES (NEW.a); RETURN NEW; END",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t1 AFTER INSERT ON s.src FOR EACH ROW EXECUTE FUNCTION to_g()",
    );
    ok(&core, TENANT, "CREATE VIEW sv AS SELECT a FROM src");
    ok(&core, TENANT, "INSERT INTO sv (a) VALUES (5)");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM gp"),
        g(&[&["5", "10"]])
    );
}

/// FK の参照アクション（ON UPDATE CASCADE・ON DELETE／UPDATE SET NULL）が書き換える子表の生成列も再計算され、
/// 孫の FK が子の生成列を参照する連鎖でも整合する。
#[test]
fn referential_actions_recompute_generated_columns_of_children() {
    let (_gd, core) = fixture("gen-p-fk");
    ok(&core, TENANT, "CREATE TABLE s.p (k INTEGER PRIMARY KEY)");
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.ch (id INTEGER PRIMARY KEY, pk INTEGER REFERENCES s.p (k) ON UPDATE CASCADE ON DELETE SET NULL, \
         dbl INTEGER GENERATED ALWAYS AS (pk * 2) STORED)",
    );
    ok(&core, TENANT, "INSERT INTO p (k) VALUES (1), (2)");
    ok(
        &core,
        TENANT,
        "INSERT INTO ch (id, pk) VALUES (10, 1), (20, 2)",
    );
    ok(&core, TENANT, "UPDATE p SET k = 5 WHERE k = 1");
    assert_eq!(
        grid(&core, TENANT, "SELECT id, pk, dbl FROM ch ORDER BY id"),
        g(&[&["10", "5", "10"], &["20", "2", "4"]])
    );
    ok(&core, TENANT, "DELETE FROM p WHERE k = 2");
    assert_eq!(
        grid(&core, TENANT, "SELECT id, pk, dbl FROM ch WHERE id = 20"),
        g(&[&["20", "NULL", "NULL"]])
    );
    // 他テナントには何も波及しない。
    assert_eq!(code(&core, OTHER, "SELECT id FROM ch"), "42P01");
}

/// 子表に BEFORE ROW トリガがあっても、参照アクションが書き換えた子行の生成列は BEFORE の後に計算される。
/// 孫の FK が子の生成列を参照する連鎖でも整合する。
#[test]
fn referential_actions_with_child_triggers_and_grandchildren() {
    let (_gd, core) = fixture("gen-p-fk2");
    ok(&core, TENANT, "CREATE TABLE s.p (k INTEGER PRIMARY KEY)");
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.ch (id INTEGER PRIMARY KEY, pk INTEGER REFERENCES s.p (k) ON UPDATE CASCADE, \
         dbl INTEGER GENERATED ALWAYS AS (pk * 2) STORED UNIQUE)",
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.gc (id INTEGER PRIMARY KEY, d INTEGER REFERENCES s.ch (dbl))",
    );
    func(&core, "noop", "BEGIN RETURN NEW; END");
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER tb BEFORE UPDATE ON s.ch FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    ok(&core, TENANT, "INSERT INTO p (k) VALUES (1)");
    ok(&core, TENANT, "INSERT INTO ch (id, pk) VALUES (10, 1)");
    ok(&core, TENANT, "INSERT INTO gc (id, d) VALUES (1, 2)");
    // 孫が子の生成列（2）を参照しているため、親のキー変更で子の生成列が 10 へ変わる更新は拒否される
    // （孫の参照先側の検査が再計算後の値に働く）。
    assert_eq!(
        code(&core, TENANT, "UPDATE p SET k = 5 WHERE k = 1"),
        "23503"
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT pk, dbl FROM ch"),
        g(&[&["1", "2"]])
    );
    ok(&core, TENANT, "DELETE FROM gc");
    ok(&core, TENANT, "UPDATE p SET k = 5 WHERE k = 1");
    assert_eq!(
        grid(&core, TENANT, "SELECT pk, dbl FROM ch"),
        g(&[&["5", "10"]])
    );
}
/// BEFORE トリガの WHEN が NEW の生成列を参照すると定義時に 42P17 で拒否される。AFTER トリガの WHEN と、
/// BEFORE トリガの WHEN が基の列・OLD の生成列を参照する形は通る。
#[test]
fn before_trigger_when_cannot_reference_new_generated_columns() {
    let (_gd, core) = fixture("gen-p-when");
    mk_g(&core);
    func(&core, "noop", "BEGIN RETURN NEW; END");
    assert_eq!(
        code(
            &core,
            TENANT,
            "CREATE TRIGGER t1 BEFORE INSERT ON s.g FOR EACH ROW WHEN (NEW.b > 1) EXECUTE FUNCTION noop()"
        ),
        "42P17"
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t2 AFTER INSERT ON s.g FOR EACH ROW WHEN (NEW.b > 1) EXECUTE FUNCTION noop()",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t3 BEFORE INSERT ON s.g FOR EACH ROW WHEN (NEW.a > 1) EXECUTE FUNCTION noop()",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER t4 BEFORE UPDATE ON s.g FOR EACH ROW WHEN (OLD.b > 1) EXECUTE FUNCTION noop()",
    );
}
