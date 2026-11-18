//! 生成列の追加 `ALTER TABLE ... ADD COLUMN ... GENERATED ALWAYS AS (<式>) STORED` の全行の原子的な計算の
//! 結合テスト（Issue #3330。ポインタ: `docs/spec/04-behavior/` の TABLE-50・RECOVER-11・TASK-345、
//! 設計記録: `docs/design/generated-column.md`）。
//!
//! 検証の軸は 5 つ。(1) 既存行つきの表への追加が全テナント・全可視性の全行を計算して成功すること
//! （空の表・`IF NOT EXISTS`・NOT NULL・fast default 列の参照・2 本目の生成列・墓標つきの表・複数アクション）。
//! (2) 追加後の書き込み・再起動・`export_schema_ddl`・索引。(3) 失敗（評価エラー・NOT NULL 違反・束縛段の拒否・
//! 権限・存在しない表・従来方式の表・明示 Tx）がいずれもカタログも行も変えないこと（他テナントの不可視な行が
//! 原因でも）。(4) 拒否の文言にテナント・値・行 id を含まないこと。(5) 結線していない経路（COPY）が `0A000`
//! のままであること（`tracked: #3329`）。

use engine::catalog::ddl_export::SchemaDdlScope;
use engine::catalog::TableSchema;
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

/// 範囲（共有 + テナント）の DDL を文字列の列として集める。
fn dump(core: &EngineCore, tenant: &str) -> Vec<String> {
    let mut out = Vec::new();
    core.export_schema_ddl(&SchemaDdlScope::shared_and_tenant(tenant), |st| {
        out.push(st.sql.to_string());
        Ok::<(), std::convert::Infallible>(())
    })
    .expect("export");
    out
}

/// 2 テナントの既存行つきの表 `t (a INTEGER, name TEXT)`。
fn mk(core: &EngineCore) {
    ok(core, TENANT, "CREATE TABLE t (a INTEGER, name TEXT)");
    ok(
        core,
        TENANT,
        "INSERT INTO t (a, name) VALUES (1, 'x'), (2, 'y')",
    );
    ok(core, OTHER, "INSERT INTO t (a, name) VALUES (10, 'z')");
}

const ADD_G: &str = "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a * 2) STORED";

#[test]
fn backfills_every_row_of_every_tenant() {
    let (_p, _gd, core) = fixture("gen-add-all");
    mk(&core);
    ok(&core, TENANT, ADD_G);
    assert_eq!(
        grid(&core, TENANT, "SELECT a, g, name FROM t ORDER BY a"),
        g(&[&["1", "2", "x"], &["2", "4", "y"]])
    );
    // 追加を実行しなかったテナントの行も計算済み（全テナントが 1 つの write txn の対象）。
    assert_eq!(
        grid(&core, OTHER, "SELECT a, g FROM t ORDER BY a"),
        g(&[&["10", "20"]])
    );
    // NULL の入力は NULL を生成する。
    ok(&core, OTHER, "INSERT INTO t (name) VALUES ('n')");
    assert_eq!(
        grid(&core, OTHER, "SELECT g FROM t WHERE name = 'n'"),
        g(&[&["NULL"]])
    );
}

#[test]
fn psql_form_with_trailing_semicolon_is_accepted() {
    let (_p, _gd, core) = fixture("gen-add-semi");
    mk(&core);
    ok(&core, TENANT, &format!("{ADD_G};"));
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t ORDER BY a"),
        g(&[&["2"], &["4"]])
    );
}

#[test]
fn empty_and_rowless_tables_accept_the_column() {
    let (_p, _gd, core) = fixture("gen-add-empty");
    // 行ストアが未作成の表。
    ok(&core, TENANT, "CREATE TABLE t (a INTEGER)");
    ok(&core, TENANT, ADD_G.replace(", name", "").as_str());
    ok(&core, TENANT, "INSERT INTO t (a) VALUES (4)");
    assert_eq!(grid(&core, TENANT, "SELECT g FROM t"), g(&[&["8"]]));
    // 行を作って消した後の空の表。
    ok(&core, TENANT, "CREATE TABLE u (a INTEGER)");
    ok(&core, TENANT, "INSERT INTO u (a) VALUES (1)");
    ok(&core, TENANT, "DELETE FROM u");
    ok(
        &core,
        TENANT,
        "ALTER TABLE u ADD COLUMN g INTEGER GENERATED ALWAYS AS (a + 1) STORED",
    );
}

#[test]
fn if_not_exists_skips_an_existing_column_without_changing_the_table() {
    let (_p, _gd, core) = fixture("gen-add-ine");
    mk(&core);
    let before = dump(&core, TENANT);
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN IF NOT EXISTS name INTEGER GENERATED ALWAYS AS (a) STORED",
    );
    assert_eq!(dump(&core, TENANT), before);
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE t ADD COLUMN name INTEGER GENERATED ALWAYS AS (a) STORED"
        ),
        "42701"
    );
    assert_eq!(dump(&core, TENANT), before);
}

#[test]
fn not_null_generated_column_needs_no_default_when_every_row_is_non_null() {
    let (_p, _gd, core) = fixture("gen-add-nn");
    mk(&core);
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a * 2) STORED NOT NULL",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t ORDER BY a"),
        g(&[&["2"], &["4"]])
    );
    assert!(dump(&core, TENANT)
        .iter()
        .any(|s| s.contains("GENERATED ALWAYS AS") && s.contains("NOT NULL")));
}

#[test]
fn fast_default_column_is_computed_from_its_fill_value() {
    let (_p, _gd, core) = fixture("gen-add-fill");
    mk(&core);
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 7",
    );
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a + c) STORED",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t ORDER BY a"),
        g(&[&["8"], &["9"]])
    );
    assert_eq!(grid(&core, OTHER, "SELECT g FROM t"), g(&[&["17"]]));
}

#[test]
fn second_generated_column_keeps_the_first_values() {
    let (_p, _gd, core) = fixture("gen-add-second");
    mk(&core);
    ok(&core, TENANT, ADD_G);
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN h INTEGER GENERATED ALWAYS AS (a + 100) STORED",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT g, h FROM t ORDER BY a"),
        g(&[&["2", "101"], &["4", "102"]])
    );
    // 生成列は生成列を参照できない。
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE t ADD COLUMN k INTEGER GENERATED ALWAYS AS (g + 1) STORED"
        ),
        "42P17"
    );
}

#[test]
fn table_with_a_dropped_column_is_accepted() {
    let (_p, _gd, core) = fixture("gen-add-tomb");
    mk(&core);
    ok(&core, TENANT, "ALTER TABLE t DROP COLUMN name");
    ok(&core, TENANT, ADD_G);
    assert_eq!(
        grid(&core, TENANT, "SELECT a, g FROM t ORDER BY a"),
        g(&[&["1", "2"], &["2", "4"]])
    );
}

#[test]
fn writes_after_the_add_compute_and_stay_consistent() {
    let (path, _gd, core) = fixture("gen-add-writes");
    mk(&core);
    ok(&core, TENANT, ADD_G);
    ok(&core, TENANT, "INSERT INTO t (a) VALUES (3)");
    ok(&core, TENANT, "UPDATE t SET a = 5 WHERE a = 3");
    // 参照しない列だけの UPDATE は、バックフィルした値が再計算と一致するため照合段で落ちない。
    ok(&core, TENANT, "UPDATE t SET name = 'w' WHERE a = 1");
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD CONSTRAINT t_a_uq UNIQUE (a)",
    );
    ok(
        &core,
        TENANT,
        "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET name = 'up'",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT a, g FROM t ORDER BY a"),
        g(&[&["1", "2"], &["2", "4"], &["5", "10"]])
    );
    // 再起動後も値と生成式が保たれ、DDL に生成式が出る。
    drop(core);
    let core = open_core(&path);
    assert_eq!(
        grid(&core, TENANT, "SELECT a, g FROM t ORDER BY a"),
        g(&[&["1", "2"], &["2", "4"], &["5", "10"]])
    );
    assert!(dump(&core, TENANT)
        .iter()
        .any(|s| s.starts_with("CREATE TABLE") && s.contains("GENERATED ALWAYS AS (")));
    // 生成列への直接指定は拒否される。
    assert_eq!(
        code(&core, TENANT, "INSERT INTO t (a, g) VALUES (1, 1)"),
        "428C9"
    );
}

#[test]
fn indexes_and_constraints_keep_working() {
    let (_p, _gd, core) = fixture("gen-add-index");
    mk(&core);
    ok(&core, TENANT, "CREATE INDEX t_a_idx ON t (a)");
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD CONSTRAINT t_a_uq UNIQUE (a)",
    );
    ok(&core, TENANT, ADD_G);
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t WHERE a = 2"),
        g(&[&["4"]])
    );
    assert_eq!(code(&core, TENANT, "INSERT INTO t (a) VALUES (1)"), "23505");
    // 追加した生成列に後から索引を張れる。
    ok(&core, TENANT, "CREATE INDEX t_g_idx ON t (g)");
    assert_eq!(
        grid(&core, TENANT, "SELECT a FROM t WHERE g = 4"),
        g(&[&["2"]])
    );
}

#[test]
fn copy_computes_the_added_generated_column() {
    // ADD COLUMN で追加した生成列を持つ表への COPY は、生成値を計算して挿入する（#3329）。
    let (_p, _gd, core) = fixture("gen-add-copy");
    mk(&core);
    ok(&core, TENANT, ADD_G);
    let policy = ctx(TENANT);
    let session = SessionState::default();
    core.begin_copy(&policy, &session, "COPY t (a) FROM STDIN")
        .and_then(|plan| match plan {
            CopyPlan::From(mut s) => {
                s.feed(b"7\n")?;
                let batch = s.finish()?;
                core.commit_copy_in(&policy, batch)
            }
            CopyPlan::To(..) => panic!("expected COPY FROM plan"),
        })
        .expect("COPY must be accepted");
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t WHERE a = 7"),
        g(&[&["14"]])
    );
}

#[test]
fn evaluation_error_on_another_tenants_row_changes_nothing() {
    let (_p, _gd, core) = fixture("gen-add-err");
    mk(&core);
    // 他テナントの行（実行者からは不可視）が原因の評価エラー。
    ok(&core, OTHER, "INSERT INTO t (a, name) VALUES (0, 'boom')");
    let before = dump(&core, TENANT);
    let sql = "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (10 / a) STORED";
    assert_eq!(code(&core, TENANT, sql), "22012");
    let m = message(&core, TENANT, sql);
    assert!(
        !m.contains(TENANT) && !m.contains(OTHER) && !m.contains("boom"),
        "{m}"
    );
    assert_eq!(dump(&core, TENANT), before);
    assert_eq!(code(&core, TENANT, "SELECT g FROM t"), "42703");
    assert_eq!(
        grid(&core, OTHER, "SELECT a, name FROM t ORDER BY a"),
        g(&[&["0", "boom"], &["10", "z"]])
    );
    // 失敗後に、同じ名前で通常の列を追加し直せる。
    ok(&core, TENANT, "ALTER TABLE t ADD COLUMN g INTEGER");
    // 後始末をした上で、同じ追加が成功する（失敗は副作用を残さない）。
    ok(&core, TENANT, "ALTER TABLE t DROP COLUMN g");
    ok(&core, OTHER, "DELETE FROM t WHERE a = 0");
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (10 / a) STORED",
    );
}

#[test]
fn integer_overflow_and_not_null_violation_change_nothing() {
    let (_p, _gd, core) = fixture("gen-add-overflow");
    mk(&core);
    let before = dump(&core, TENANT);
    ok(
        &core,
        OTHER,
        "INSERT INTO t (a, name) VALUES (2147483647, 'big')",
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a * 2) STORED"
        ),
        "22003"
    );
    assert_eq!(dump(&core, TENANT), before);
    // NOT NULL の生成列で NULL になる行がある。文言は列名だけ。
    ok(&core, OTHER, "DELETE FROM t WHERE name = 'big'");
    ok(&core, OTHER, "INSERT INTO t (name) VALUES ('zzq')");
    let sql = "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a * 2) STORED NOT NULL";
    assert_eq!(code(&core, TENANT, sql), "23502");
    let m = message(&core, TENANT, sql);
    assert!(
        !m.contains(TENANT) && !m.contains(OTHER) && !m.contains("zzq"),
        "{m}"
    );
    assert_eq!(dump(&core, TENANT), before);
    assert_eq!(code(&core, TENANT, "SELECT g FROM t"), "42703");
}

#[test]
fn binding_stage_rejections_share_the_create_table_sqlstates() {
    let (_p, _gd, core) = fixture("gen-add-bind");
    mk(&core);
    let before = dump(&core, TENANT);
    for (sql, expected) in [
        // 構文段。
        (
            "ALTER TABLE t ADD COLUMN g INTEGER DEFAULT 1 GENERATED ALWAYS AS (a) STORED",
            "42601",
        ),
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a) VIRTUAL",
            "42601",
        ),
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (a) STORED UNIQUE",
            "42601",
        ),
        (
            "ALTER TABLE t ADD COLUMN g VECTOR(2) GENERATED ALWAYS AS (a) STORED",
            "0A000",
        ),
        (
            "ALTER TABLE t ADD COLUMN g TIMESTAMPTZ GENERATED ALWAYS AS (now()) STORED",
            "42P17",
        ),
        // 束縛段。
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (zzz + 1) STORED",
            "42703",
        ),
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (g + 1) STORED",
            "42P17",
        ),
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (name) STORED",
            "42804",
        ),
        (
            "ALTER TABLE t ADD COLUMN g NUMERIC(10, 2) GENERATED ALWAYS AS (a) STORED",
            "0A000",
        ),
        (
            "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (id) STORED",
            "42703",
        ),
    ] {
        assert_eq!(code(&core, TENANT, sql), expected, "{sql}");
        assert_eq!(dump(&core, TENANT), before, "{sql}");
    }
    assert_eq!(code(&core, TENANT, "SELECT g FROM t"), "42703");
}

#[test]
fn permission_and_existence_are_decided_before_the_expression() {
    let (_p, _gd, core) = fixture("gen-add-gate");
    mk(&core);
    ok(&core, TENANT, "CREATE VIEW vw AS SELECT a FROM t");
    // 権限なしは表の有無・式の正否によらず同じ `42501`。
    for sql in [
        ADD_G,
        "ALTER TABLE nope ADD COLUMN g INTEGER GENERATED ALWAYS AS (zzz) STORED",
        "ALTER TABLE t ADD COLUMN g INTEGER GENERATED ALWAYS AS (zzz) STORED",
    ] {
        let mut session = SessionState::default();
        let err = core
            .execute_sql_in_session(&ctx(TENANT), &mut session, sql)
            .expect_err("no DDL permission");
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
    // 存在しない表・ビューは式の不正より先に返る。
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE nope ADD COLUMN g INTEGER GENERATED ALWAYS AS (zzz) STORED"
        ),
        "42P01"
    );
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE vw ADD COLUMN g INTEGER GENERATED ALWAYS AS (zzz) STORED"
        ),
        "42809"
    );
}

#[test]
fn legacy_row_identity_table_is_rejected_with_a_fixed_message() {
    // NoSQL 表層が作る従来方式の表は、生成列の計算点が無いため CREATE TABLE と同じ `0A000`（intentional）。
    // intentional: docs/design/generated-column.md
    let path = unique_db_path("gen-add-legacy");
    let _gd = CleanupGuard(path.clone());
    // 開いた後に作った表は従来方式のまま（開く時点の移行は既存の表だけが対象）。
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            "legacy",
            vec![engine::catalog::ColumnDef::new(
                "a",
                engine::catalog::ColumnType::Integer,
                true,
            )],
        ))
        .expect("create legacy table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let sql = "ALTER TABLE legacy ADD COLUMN g INTEGER GENERATED ALWAYS AS (a) STORED";
    assert_eq!(code(&core, TENANT, sql), "0A000");
    let m = message(&core, TENANT, sql);
    assert!(!m.contains(TENANT) && !m.contains("legacy"), "{m}");
    assert_eq!(code(&core, TENANT, "SELECT g FROM legacy"), "42703");
}

#[test]
fn temporary_is_rejected_and_explicit_transaction_applies_atomically() {
    let (_p, _gd, core) = fixture("gen-add-forms");
    mk(&core);
    let before = dump(&core, TENANT);
    // 一時表（セッション内で閉じる）。
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx(TENANT);
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (a INTEGER)")
        .expect("create temp");
    let err = core
        .execute_sql_in_session(
            &c,
            &mut session,
            "ALTER TABLE tt ADD COLUMN g INTEGER GENERATED ALWAYS AS (a) STORED",
        )
        .expect_err("temp table generated column");
    assert_eq!(err.wire_code(), "0A000");
    // 明示トランザクションの中の DDL（Issue #3313・#3314）。行のある表への生成列の追加は、全行の計算に
    // Tx の期限の確認点が結線されたため Tx 内で通る。ROLLBACK は痕跡を残さず、COMMIT は原子的に確定する。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, ADD_G)
        .expect("scan DDL on a non-empty table in a transaction");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(dump(&core, TENANT), before);
    assert_eq!(code(&core, TENANT, "SELECT g FROM t"), "42703");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, ADD_G)
        .expect("scan DDL on a non-empty table in a transaction");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    ok(&core, TENANT, "SELECT g FROM t");
    // 以降の検査は空の表 `e` で行う。
    // 空の表への追加は走査が無いため、COMMIT で原子的に確定する。ROLLBACK は痕跡を残さない。
    ok(&core, TENANT, "CREATE TABLE e (a INTEGER)");
    let add_e = "ALTER TABLE e ADD COLUMN g INTEGER GENERATED ALWAYS AS (a * 2) STORED";
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, add_e)
        .expect("generated ADD COLUMN on an empty table in explicit transaction");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(code(&core, TENANT, "SELECT g FROM e"), "42703");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, add_e)
        .expect("generated ADD COLUMN on an empty table in explicit transaction");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    ok(&core, TENANT, "INSERT INTO e (a) VALUES (3)");
    assert_eq!(grid(&core, TENANT, "SELECT a, g FROM e"), g(&[&["3", "6"]]));
}

#[test]
fn multiple_actions_apply_in_one_transaction() {
    let (_p, _gd, core) = fixture("gen-add-multi");
    mk(&core);
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN b INTEGER DEFAULT 1, \
         ADD COLUMN g INTEGER GENERATED ALWAYS AS (a + b) STORED",
    );
    assert_eq!(
        grid(&core, TENANT, "SELECT g FROM t ORDER BY a"),
        g(&[&["2"], &["3"]])
    );
    // 後続の SET NOT NULL は計算済みの値に対して検査される。
    ok(
        &core,
        TENANT,
        "ALTER TABLE t ADD COLUMN h INTEGER GENERATED ALWAYS AS (a) STORED, \
         ALTER COLUMN h SET NOT NULL",
    );
    // 後続のアクションが失敗すると、生成列の追加も行の書き直しも破棄される。
    let before = dump(&core, TENANT);
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE t ADD COLUMN k INTEGER GENERATED ALWAYS AS (a) STORED, \
             ALTER COLUMN nope SET NOT NULL"
        ),
        "42703"
    );
    assert_eq!(dump(&core, TENANT), before);
    assert_eq!(code(&core, TENANT, "SELECT k FROM t"), "42703");
    // 評価エラーも全体を破棄する。
    assert_eq!(
        code(
            &core,
            TENANT,
            "ALTER TABLE t ADD COLUMN k INTEGER GENERATED ALWAYS AS (1 / (a - 1)) STORED, \
             ADD COLUMN m INTEGER"
        ),
        "22012"
    );
    assert_eq!(dump(&core, TENANT), before);
}
