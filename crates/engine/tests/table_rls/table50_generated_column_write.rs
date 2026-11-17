//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の書き込み（INSERT・UPDATE・UPSERT）の結合テスト
//! （Issue #3328。ポインタ: `docs/spec/04-behavior/` の TABLE-50・RECOVER-11・TASK-345、
//! 設計記録: `docs/design/generated-column.md`）。
//!
//! 検証の軸は 6 つ。(1) INSERT の各形（単一行・複数行・`INSERT ... SELECT`・列リスト省略・`DEFAULT`）で
//! 生成列が計算され `RETURNING` が計算後の値を返すこと。(2) UPDATE の各形（式・述語形・`UPDATE ... FROM`・
//! 分割実行）で参照列が変わる行だけ再計算されること。(3) UPSERT の挿入側・`DO UPDATE`・生成列への
//! `ON CONFLICT`。(4) 生成列への値の指定が `428C9` で拒否されること（`DEFAULT` は受理）。
//! (5) 計算後の値に対して UNIQUE・CHECK・FK・RLS WITH CHECK が働くこと、評価エラーで行が残らないこと。
//! (6) COPY・ビュー・DO ブロックも同じ計算点を通ること（トリガ・NoSQL・FK の連鎖は `table50_generated_column_paths`）。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
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

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
}

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    (path, guard, core)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
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

fn message(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.to_string(),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        other => format!("{other:?}"),
    }
}

/// 結果行を文字列の表にする（SELECT・RETURNING 共通）。
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

fn count(core: &EngineCore, tenant: &str, table: &str) -> usize {
    grid(core, tenant, &format!("SELECT a FROM {table}")).len()
}

fn mk(core: &EngineCore) {
    ok(
        core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, name TEXT)",
    );
}

#[test]
fn insert_forms_compute_the_generated_column() {
    let (_p, _gd, core) = fixture("gen-w-insert");
    mk(&core);
    ok(&core, TENANT, "INSERT INTO g (a, name) VALUES (1, 'x')");
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (2), (3)");
    ok(&core, TENANT, "INSERT INTO g (a, b) VALUES (4, DEFAULT)");
    ok(&core, TENANT, "INSERT INTO g (a) SELECT 5");
    ok(&core, TENANT, "INSERT INTO g VALUES (6, DEFAULT, 'y')");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[
            &["1", "2"],
            &["2", "4"],
            &["3", "6"],
            &["4", "8"],
            &["5", "10"],
            &["6", "12"]
        ])
    );
    // NULL の入力は NULL を生成する。
    ok(&core, TENANT, "INSERT INTO g (name) VALUES ('n')");
    assert_eq!(
        grid(&core, TENANT, "SELECT b FROM g WHERE name = 'n'"),
        g(&[&["NULL"]])
    );
}

#[test]
fn insert_returning_shows_computed_values() {
    let (_p, _gd, core) = fixture("gen-w-ret");
    mk(&core);
    assert_eq!(
        grid(&core, TENANT, "INSERT INTO g (a) VALUES (7) RETURNING a, b"),
        g(&[&["7", "14"]])
    );
    assert_eq!(
        grid(
            &core,
            TENANT,
            "INSERT INTO g (a) VALUES (1), (2) RETURNING b"
        ),
        g(&[&["2"], &["4"]])
    );
}

#[test]
fn explicit_values_for_generated_columns_are_rejected_with_428c9() {
    let (_p, _gd, core) = fixture("gen-w-428");
    mk(&core);
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1)");
    for sql in [
        "INSERT INTO g (a, b) VALUES (1, 2)",
        "INSERT INTO g (a, b) VALUES (1, NULL)",
        "INSERT INTO g (a, b) VALUES (1, 'not an int')",
        "INSERT INTO g VALUES (1, 2, 'x')",
        "INSERT INTO g (a, b) SELECT 1, 2",
        "UPDATE g SET b = 5",
        "UPDATE g SET b = 5 WHERE a = 1",
        "UPDATE g SET b = a + 1 WHERE a = 1",
        "INSERT INTO g (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = 3",
    ] {
        let c = run(&core, TENANT, sql);
        let c = match c {
            Ok(o) => panic!("{sql} must fail, got {o:?}"),
            Err(e) => e.wire_code(),
        };
        assert!(c == "428C9" || c == "42P10" || c == "0A000", "{sql}: {c}");
        if sql.starts_with("INSERT INTO g (a, b)") || sql.starts_with("UPDATE") {
            assert_eq!(c, "428C9", "{sql}");
        }
    }
    // 拒否した文は何も残さない。
    assert_eq!(grid(&core, TENANT, "SELECT a, b FROM g"), g(&[&["1", "2"]]));
}

#[test]
fn update_recomputes_only_when_referenced_columns_change() {
    let (_p, _gd, core) = fixture("gen-w-update");
    mk(&core);
    ok(
        &core,
        TENANT,
        "INSERT INTO g (a, name) VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    );
    ok(&core, TENANT, "UPDATE g SET a = 10 WHERE a = 1");
    ok(&core, TENANT, "UPDATE g SET name = 'q' WHERE a = 2");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b, name FROM g ORDER BY a"),
        g(&[&["2", "4", "q"], &["3", "6", "z"], &["10", "20", "x"]])
    );
    // 式・RETURNING。
    assert_eq!(
        grid(
            &core,
            TENANT,
            "UPDATE g SET a = a + 1 WHERE a = 3 RETURNING a, b"
        ),
        g(&[&["4", "8"]])
    );
    // `SET b = DEFAULT` は受理され値は式のまま。
    ok(&core, TENANT, "UPDATE g SET b = DEFAULT WHERE a = 2");
    ok(
        &core,
        TENANT,
        "UPDATE g SET b = DEFAULT, name = 'k' WHERE a = 2",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b, name FROM g WHERE a = 2"),
        g(&[&["2", "4", "k"]])
    );
}

#[test]
fn update_from_and_partitioned_update_compute() {
    let (_p, _gd, core) = fixture("gen-w-update-from");
    mk(&core);
    ok(&core, TENANT, "CREATE TABLE src (k INTEGER, v INTEGER)");
    ok(
        &core,
        TENANT,
        "INSERT INTO src (k, v) VALUES (1, 100), (2, 200)",
    );
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1), (2), (3)");
    ok(
        &core,
        TENANT,
        "UPDATE g SET a = src.v FROM src WHERE g.a = src.k",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[&["3", "6"], &["100", "200"], &["200", "400"]])
    );
}

/// 分割実行 UPDATE（台帳が必要なため既定の台帳モードの core を使う）。各チャンクで生成列が計算される。
#[test]
fn partitioned_update_computes_in_every_chunk() {
    let path = unique_db_path("gen-w-partitioned");
    let _gd = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, name TEXT)",
    );
    ok(
        &core,
        TENANT,
        "INSERT INTO g (a) VALUES (1), (2), (3) USING OPERATION_ID 'ins1'",
    );
    ok(
        &core,
        TENANT,
        "UPDATE g SET a = a + 1 WHERE a > 0 USING OPERATION_ID 'pp1' PARTITIONED CHUNK 1",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[&["2", "4"], &["3", "6"], &["4", "8"]])
    );
}

#[test]
fn upsert_computes_on_insert_and_on_do_update() {
    let (_p, _gd, core) = fixture("gen-w-upsert");
    ok(
        &core,
        TENANT,
        "CREATE TABLE u (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)",
    );
    ok(&core, TENANT, "INSERT INTO u (k, a) VALUES (1, 5)");
    // 挿入側。
    ok(
        &core,
        TENANT,
        "INSERT INTO u (k, a) VALUES (2, 7) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a",
    );
    // DO UPDATE 側の再計算。
    ok(
        &core,
        TENANT,
        "INSERT INTO u (k, a) VALUES (1, 50) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT k, a, b FROM u ORDER BY k"),
        g(&[&["1", "50", "100"], &["2", "7", "14"]])
    );
    // RETURNING は計算後の値。
    assert_eq!(
        grid(
            &core,
            TENANT,
            "INSERT INTO u (k, a) VALUES (1, 9) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a RETURNING a, b"
        ),
        g(&[&["9", "18"]])
    );
    // 生成列への ON CONFLICT 対象は計算後の値で判定される。
    ok(&core, TENANT, "CREATE UNIQUE INDEX u_b ON u (b)");
    ok(
        &core,
        TENANT,
        "INSERT INTO u (k, a) VALUES (3, 9) ON CONFLICT (b) DO NOTHING",
    );
    assert_eq!(count_rows(&core, "u"), 2);
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO u (k, a) VALUES (1, 1) ON CONFLICT (k) DO UPDATE SET b = 1"
        ),
        "428C9"
    );
}

fn count_rows(core: &EngineCore, table: &str) -> usize {
    grid(core, TENANT, &format!("SELECT k FROM {table}")).len()
}

#[test]
fn constraints_see_the_computed_value() {
    let (_p, _gd, core) = fixture("gen-w-constraints");
    ok(&core, TENANT, "CREATE TABLE parent (p INTEGER PRIMARY KEY)");
    ok(&core, TENANT, "INSERT INTO parent (p) VALUES (2), (4)");
    ok(
        &core,
        TENANT,
        "CREATE TABLE c (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED, UNIQUE (b), CHECK (b < 100), FOREIGN KEY (b) REFERENCES parent (p))",
    );
    ok(&core, TENANT, "INSERT INTO c (a) VALUES (1)");
    // UNIQUE（計算後）。
    assert_eq!(code(&core, TENANT, "INSERT INTO c (a) VALUES (1)"), "23505");
    // FK（計算後。b = 6 は親に無い）。
    assert_eq!(code(&core, TENANT, "INSERT INTO c (a) VALUES (3)"), "23503");
    // CHECK（計算後）。
    ok(&core, TENANT, "INSERT INTO parent (p) VALUES (200)");
    assert_eq!(
        code(&core, TENANT, "INSERT INTO c (a) VALUES (100)"),
        "23514"
    );
    assert_eq!(count(&core, TENANT, "c"), 1);
    // UPDATE でも計算後の値で検査される。
    assert_eq!(code(&core, TENANT, "UPDATE c SET a = 3"), "23503");
    assert_eq!(grid(&core, TENANT, "SELECT a, b FROM c"), g(&[&["1", "2"]]));
}

#[test]
fn fk_to_a_generated_parent_column_is_checked_on_update() {
    let (_p, _gd, core) = fixture("gen-w-fk-parent");
    ok(
        &core,
        TENANT,
        "CREATE TABLE par (a INTEGER, k INTEGER GENERATED ALWAYS AS (a * 10) STORED UNIQUE)",
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE kid (r INTEGER REFERENCES par (k))",
    );
    ok(&core, TENANT, "INSERT INTO par (a) VALUES (1)");
    ok(&core, TENANT, "INSERT INTO kid (r) VALUES (10)");
    // 参照先の生成列が変わる UPDATE は、子が参照しているため拒否される。
    let c = code(&core, TENANT, "UPDATE par SET a = 2");
    assert!(c == "23503" || c == "0A000", "{c}");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, k FROM par"),
        g(&[&["1", "10"]])
    );
}

#[test]
fn evaluation_errors_fail_the_whole_statement() {
    let (_p, _gd, core) = fixture("gen-w-errors");
    ok(
        &core,
        TENANT,
        "CREATE TABLE e (a INTEGER, q INTEGER GENERATED ALWAYS AS (100 / a) STORED)",
    );
    ok(&core, TENANT, "INSERT INTO e (a) VALUES (5)");
    assert_eq!(
        code(&core, TENANT, "INSERT INTO e (a) VALUES (1), (0)"),
        "22012"
    );
    assert_eq!(code(&core, TENANT, "UPDATE e SET a = 0"), "22012");
    assert_eq!(count(&core, TENANT, "e"), 1);
    ok(
        &core,
        TENANT,
        "CREATE TABLE nn (a INTEGER, c INTEGER NOT NULL GENERATED ALWAYS AS (a + 1) STORED)",
    );
    assert_eq!(
        code(&core, TENANT, "INSERT INTO nn (a) VALUES (NULL)"),
        "23502"
    );
    ok(&core, TENANT, "INSERT INTO nn (a) VALUES (1)");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, c FROM nn"),
        g(&[&["1", "2"]])
    );
}

#[test]
fn rls_with_check_sees_the_computed_value() {
    let (_p, _gd, core) = fixture("gen-w-rls");
    mk(&core);
    ok(&core, TENANT, "ALTER TABLE g ENABLE ROW LEVEL SECURITY");
    ok(
        &core,
        TENANT,
        "CREATE POLICY p ON g FOR ALL USING (true) WITH CHECK (b < 10)",
    );
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1)");
    assert_eq!(code(&core, TENANT, "INSERT INTO g (a) VALUES (6)"), "42501");
    assert_eq!(code(&core, TENANT, "UPDATE g SET a = 6"), "42501");
    assert_eq!(count(&core, TENANT, "g"), 1);
}

#[test]
fn tenants_do_not_see_each_others_rows_or_values() {
    let (_p, _gd, core) = fixture("gen-w-tenants");
    mk(&core);
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (1)");
    ok(&core, OTHER, "INSERT INTO g (a) VALUES (2)");
    assert_eq!(grid(&core, TENANT, "SELECT b FROM g"), g(&[&["2"]]));
    assert_eq!(grid(&core, OTHER, "SELECT b FROM g"), g(&[&["4"]]));
    let m = message(&core, TENANT, "INSERT INTO g (a, b) VALUES (1, 1)");
    assert!(!m.contains(TENANT) && !m.contains(OTHER), "{m}");
}

#[test]
fn explicit_transactions_compute_and_roll_back() {
    let (_p, _gd, core) = fixture("gen-w-txn");
    mk(&core);
    let mut session = SessionState::default();
    let c = ctx(TENANT);
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO g (a) VALUES (4)",
        "UPDATE g SET a = 5",
    ] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, TENANT, "g"), 0);
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "INSERT INTO g (a) VALUES (4)", "COMMIT"] {
        core.execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(grid(&core, TENANT, "SELECT b FROM g"), g(&[&["8"]]));
}

#[test]
fn computed_values_survive_restart() {
    let (path, _gd, core) = fixture("gen-w-restart");
    mk(&core);
    ok(&core, TENANT, "INSERT INTO g (a) VALUES (21)");
    drop(core);
    let reopened = open_core(&path);
    assert_eq!(grid(&reopened, TENANT, "SELECT b FROM g"), g(&[&["42"]]));
    ok(&reopened, TENANT, "UPDATE g SET a = 22");
    assert_eq!(grid(&reopened, TENANT, "SELECT b FROM g"), g(&[&["44"]]));
}

#[test]
fn text_and_bare_null_expressions_compute() {
    let (_p, _gd, core) = fixture("gen-w-text");
    ok(
        &core,
        TENANT,
        "CREATE TABLE t (a TEXT, up TEXT GENERATED ALWAYS AS (upper(a)) STORED, z INTEGER GENERATED ALWAYS AS (NULL) STORED)",
    );
    ok(&core, TENANT, "INSERT INTO t (a) VALUES ('abc')");
    assert_eq!(
        grid(&core, TENANT, "SELECT up, z FROM t"),
        g(&[&["ABC", "NULL"]])
    );
}

/// ビュー経由・COPY FROM（Issue #3329）も SQL 表層と同じ計算点で生成列を計算する。ビュー経由の生成列への
/// 明示値と COPY の列リストの生成列は `428C9`（行・台帳を残さない）。
#[test]
fn view_and_copy_paths_compute_the_generated_column() {
    let (_p, _gd, core) = fixture("gen-w-view-copy");
    mk(&core);
    ok(&core, TENANT, "CREATE VIEW gv AS SELECT a, b FROM g");
    ok(&core, TENANT, "INSERT INTO gv (a) VALUES (1)");
    assert_eq!(
        code(&core, TENANT, "INSERT INTO gv (a, b) VALUES (2, 9)"),
        "428C9"
    );
    ok(&core, TENANT, "UPDATE gv SET a = 3");
    assert_eq!(grid(&core, TENANT, "SELECT a, b FROM g"), g(&[&["3", "6"]]));
    // COPY FROM STDIN。
    let policy = ctx(TENANT);
    let session = SessionState::default();
    let copy = core
        .begin_copy(&policy, &session, "COPY g (a) FROM STDIN")
        .and_then(|plan| match plan {
            CopyPlan::From(mut s) => {
                s.feed(b"5\n")?;
                let batch = s.finish()?;
                core.commit_copy_in(&policy, batch)
            }
            CopyPlan::To(..) => panic!("expected COPY FROM plan"),
        });
    copy.expect("COPY must compute the generated column");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM g ORDER BY a"),
        g(&[&["3", "6"], &["5", "10"]])
    );
    // 列リストの生成列は CopyInResponse の前に 428C9。
    let err = core
        .begin_copy(&policy, &session, "COPY g (a, b) FROM STDIN")
        .expect_err("generated column in COPY column list");
    assert_eq!(err.wire_code(), "428C9");
    assert_eq!(count(&core, TENANT, "g"), 2);
}
/// 台帳（RECOVER-11 のポインタ）。生成列は内容照合ハッシュの材料から除くため、省略形と `DEFAULT` 形の再送が
/// 一致する。評価エラーで失敗した文は台帳を残さない。
#[test]
fn ledger_resend_matches_across_omitted_and_default_forms() {
    let path = unique_db_path("gen-w-ledger");
    let _gd = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (100 / a) STORED)",
    );
    ok(
        &core,
        TENANT,
        "INSERT INTO g (a) VALUES (5) USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO g (a) VALUES (5) USING OPERATION_ID 'o1'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO g (a, b) VALUES (5, DEFAULT) USING OPERATION_ID 'o1'"
        ),
        "23505"
    );
    // 評価エラーで失敗した文は台帳を残さず、同じ operation_id で直した文が新規として通る。
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO g (a) VALUES (0) USING OPERATION_ID 'o2'"
        ),
        "22012"
    );
    ok(
        &core,
        TENANT,
        "INSERT INTO g (a) VALUES (4) USING OPERATION_ID 'o2'",
    );
    assert_eq!(count(&core, TENANT, "g"), 2);
}

/// DO ブロック内の INSERT は通常の SQL 表層の経路（`execute_insert_form`）を通るため結線済みで、生成列が計算される。
#[test]
fn do_block_inserts_compute_through_the_sql_surface() {
    let (_p, _gd, core) = fixture("gen-w-do");
    mk(&core);
    ok(
        &core,
        TENANT,
        "DO $$ BEGIN INSERT INTO g (a) VALUES (1); END $$",
    );
    assert_eq!(grid(&core, TENANT, "SELECT a, b FROM g"), g(&[&["1", "2"]]));
}
/// スキーマ修飾の表でも書き込み先の表名（結線済みの区間のキー）が一致して計算される。トリガを持つ表への
/// 書き込みも、BEFORE ROW の後に計算される（Issue #3329）。
#[test]
fn schema_qualified_tables_compute_with_and_without_triggers() {
    let (_p, _gd, core) = fixture("gen-w-schema-trigger");
    ok(&core, TENANT, "CREATE SCHEMA s");
    ok(
        &core,
        TENANT,
        "CREATE TABLE s.t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED)",
    );
    ok(&core, TENANT, "INSERT INTO s.t (a) VALUES (1)");
    ok(&core, TENANT, "UPDATE s.t SET a = 2");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM s.t"),
        g(&[&["2", "3"]])
    );
    ok(
        &core,
        TENANT,
        "CREATE FUNCTION noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
    ok(
        &core,
        TENANT,
        "CREATE TRIGGER trg BEFORE INSERT OR UPDATE ON s.t FOR EACH ROW EXECUTE FUNCTION noop()",
    );
    ok(&core, TENANT, "INSERT INTO s.t (a) VALUES (5)");
    ok(&core, TENANT, "UPDATE s.t SET a = a + 5 WHERE a = 5");
    assert_eq!(
        grid(&core, TENANT, "SELECT a, b FROM s.t ORDER BY a"),
        g(&[&["2", "3"], &["10", "11"]])
    );
}
/// 行つきの論理ダンプ（INSERT 形式）の往復（Issue #3328）。生成列はデータ出力から除かれ、
/// 復元先の INSERT が計算し直す。再ダンプは元とバイト単位で一致する（COPY 形式は次のテスト）。
#[test]
fn row_bearing_logical_dump_round_trips_in_insert_format() {
    use engine::catalog::ddl_export::{
        LogicalDumpItem, SchemaDdlScope, TableDataFormat, TableDataPart,
    };
    let (_p1, _g1, src) = fixture("gen-w-dump-src");
    mk(&src);
    ok(
        &src,
        TENANT,
        "INSERT INTO g (a, name) VALUES (1, 'x'), (2, 'y')",
    );
    ok(&src, TENANT, "INSERT INTO g (name) VALUES ('n')");
    let dump = |core: &EngineCore| -> Vec<(bool, String)> {
        let mut out = Vec::new();
        core.export_logical_dump(
            &SchemaDdlScope::shared_and_tenant(TENANT),
            TableDataFormat::Insert,
            |item| {
                out.push(match item {
                    LogicalDumpItem::Schema(st) => (false, st.sql.to_string()),
                    LogicalDumpItem::Data(d) => {
                        assert_eq!(d.part, TableDataPart::Insert);
                        (true, d.text.to_string())
                    }
                });
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .expect("dump");
        out
    };
    let pieces = dump(&src);
    assert!(
        pieces
            .iter()
            .any(|(data, sql)| *data && sql.contains("INSERT INTO")),
        "{pieces:?}"
    );
    for (data, sql) in &pieces {
        if *data {
            assert!(
                !sql.contains("\"b\""),
                "generated column must be omitted: {sql}"
            );
        }
    }
    let (_p2, _g2, dst) = fixture("gen-w-dump-dst");
    for (_, sql) in &pieces {
        ok(&dst, TENANT, sql);
    }
    assert_eq!(
        grid(&dst, TENANT, "SELECT a, b, name FROM g ORDER BY name"),
        grid(&src, TENANT, "SELECT a, b, name FROM g ORDER BY name")
    );
    assert_eq!(dump(&dst), pieces);
}

/// COPY 形式の行つき論理ダンプの往復（Issue #3329）。生成列は COPY の列リストから除かれ、復元先の COPY が計算し直す。
/// 再ダンプは元と一致する。
#[test]
fn row_bearing_logical_dump_round_trips_in_copy_format() {
    use engine::catalog::ddl_export::{
        LogicalDumpItem, SchemaDdlScope, TableDataFormat, TableDataPart,
    };
    let (_p1, _g1, src) = fixture("gen-w-dump-copy-src");
    mk(&src);
    ok(
        &src,
        TENANT,
        "INSERT INTO g (a, name) VALUES (1, 'x'), (2, 'y')",
    );
    ok(&src, TENANT, "INSERT INTO g (name) VALUES ('n')");
    let dump = |core: &EngineCore| -> Vec<(TableDataPart, bool, String)> {
        let mut out = Vec::new();
        core.export_logical_dump(
            &SchemaDdlScope::shared_and_tenant(TENANT),
            TableDataFormat::Copy,
            |item| {
                out.push(match item {
                    LogicalDumpItem::Schema(st) => {
                        (TableDataPart::Insert, false, st.sql.to_string())
                    }
                    LogicalDumpItem::Data(d) => (d.part, true, d.text.to_string()),
                });
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .expect("dump");
        out
    };
    let pieces = dump(&src);
    let header = pieces
        .iter()
        .find(|(part, data, _)| *data && *part == TableDataPart::CopyHeader)
        .expect("COPY header");
    assert!(!header.2.contains("\"b\""), "{}", header.2);
    let (_p2, _g2, dst) = fixture("gen-w-dump-copy-dst");
    let mut copy: Option<(String, Vec<u8>)> = None;
    for (part, data, text) in &pieces {
        if !*data {
            ok(&dst, TENANT, text);
            continue;
        }
        match part {
            TableDataPart::CopyHeader => copy = Some((text.clone(), Vec::new())),
            TableDataPart::CopyRow => {
                let (_, buf) = copy.as_mut().expect("header before rows");
                buf.extend_from_slice(text.as_bytes());
                buf.push(b'\n');
            }
            TableDataPart::CopyEnd => {
                let (sql, buf) = copy.take().expect("header before end");
                let policy = ctx(TENANT);
                let CopyPlan::From(mut s) = dst
                    .begin_copy(&policy, &SessionState::default(), &sql)
                    .unwrap_or_else(|e| panic!("{sql} must begin: {e}"))
                else {
                    panic!("expected COPY FROM");
                };
                s.feed(&buf).expect("feed");
                let batch = s.finish().expect("finish");
                dst.commit_copy_in(&policy, batch).expect("commit copy");
            }
            TableDataPart::Insert => {
                ok(&dst, TENANT, text);
            }
        }
    }
    assert_eq!(
        grid(&dst, TENANT, "SELECT a, b, name FROM g ORDER BY name"),
        grid(&src, TENANT, "SELECT a, b, name FROM g ORDER BY name")
    );
    assert_eq!(dump(&dst), pieces);
}

/// `EXCLUDED` が生成列を参照する場合の NOT NULL 検査は計算後の値で行う（VALUES 形・`INSERT ... SELECT` 形）。
#[test]
fn upsert_excluded_generated_column_is_checked_after_computation() {
    let (_p, _gd, core) = fixture("gen-w-excluded");
    ok(
        &core,
        TENANT,
        "CREATE TABLE e (k INTEGER PRIMARY KEY, a INTEGER, g INTEGER GENERATED ALWAYS AS (a * 2) STORED, n INTEGER NOT NULL)",
    );
    ok(&core, TENANT, "INSERT INTO e (k, a, n) VALUES (1, 1, 0)");
    ok(
        &core,
        TENANT,
        "INSERT INTO e (k, a, n) VALUES (1, 5, 0) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.g",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT k, g, n FROM e"),
        g(&[&["1", "2", "10"]])
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE src (k INTEGER PRIMARY KEY, a INTEGER)",
    );
    ok(&core, TENANT, "INSERT INTO src (k, a) VALUES (1, 7)");
    ok(
        &core,
        TENANT,
        "INSERT INTO e (k, a, n) SELECT k, a, 0 FROM src ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.g",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT k, g, n FROM e"),
        g(&[&["1", "2", "14"]])
    );
}

/// 生成列の衝突キーは計算後の値で文内重複を検査する（同じ既存行を 2 回更新して成功させない）。
#[test]
fn upsert_in_statement_duplicates_on_generated_target_are_rejected() {
    let (_p, _gd, core) = fixture("gen-w-dup");
    ok(
        &core,
        TENANT,
        "CREATE TABLE d (k INTEGER PRIMARY KEY, a INTEGER, g INTEGER GENERATED ALWAYS AS (a * 2) STORED UNIQUE)",
    );
    ok(&core, TENANT, "INSERT INTO d (k, a) VALUES (1, 1)");
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO d (k, a) VALUES (2, 1), (3, 1) ON CONFLICT (g) DO UPDATE SET a = EXCLUDED.a"
        ),
        "22000"
    );
    ok(
        &core,
        TENANT,
        "CREATE TABLE s (k INTEGER PRIMARY KEY, a INTEGER)",
    );
    ok(&core, TENANT, "INSERT INTO s (k, a) VALUES (2, 1), (3, 1)");
    assert_eq!(
        code(
            &core,
            TENANT,
            "INSERT INTO d (k, a) SELECT k, a FROM s ON CONFLICT (g) DO UPDATE SET a = EXCLUDED.a"
        ),
        "21000"
    );
    // DO NOTHING は先勝ち。
    ok(
        &core,
        TENANT,
        "INSERT INTO d (k, a) SELECT k, a FROM s ON CONFLICT (g) DO NOTHING",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT k, a, g FROM d ORDER BY k"),
        g(&[&["1", "1", "2"]])
    );
}

/// 生成列への `SET g = DEFAULT` は何も代入しない項で、台帳の内容照合ハッシュの材料から除く
/// （`SET g = DEFAULT, name = ..` の再送が `SET name = ..` と同じ文として一致する。`id` 指定形は
/// 束縛後の SET 列でハッシュするため、述語形も同じ材料に揃える）。
#[test]
fn update_set_generated_default_is_not_hash_material() {
    let path = unique_db_path("gen-w-ledger-upd");
    let _gd = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    mk(&core);
    ok(
        &core,
        TENANT,
        "INSERT INTO g (a, name) VALUES (1, 'x') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        TENANT,
        "UPDATE g SET b = DEFAULT, name = 'k' WHERE a = 1 USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "UPDATE g SET name = 'k' WHERE a = 1 USING OPERATION_ID 'u1'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "UPDATE g SET name = 'z' WHERE a = 1 USING OPERATION_ID 'u1'"
        ),
        "22023"
    );
}
