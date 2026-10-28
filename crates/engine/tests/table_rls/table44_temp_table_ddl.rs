//! `CREATE TEMP TABLE` の構文と `ON COMMIT` 動作（TABLE-44・Issue #1683）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-44（SQL-23・SQL-31・SQL-46 を併用）。
//!
//! `EngineCore::execute_sql_in_session`／`execute_sql_in_txn`／`execute_parsed_in_session` を
//! production 経路として使う。一時表への DML は `table44_temp_table_dml.rs`（#1684）が担い、ここでは DDL の受理・
//! `ON COMMIT` の可視な結果（`DROP` の消滅・名前解決）・DDL 権限ゲートの対象外・未対応の形の
//! fail-closed な拒否・上限・セッション分離を検証する。`DELETE ROWS` の行の消え方は
//! `sql/temp_on_commit_tests.rs`・`table44_temp_table_dml.rs` が担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::temp_table_limits::TempTableLimits;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 永続表 `docs(embedding VECTOR(2))` を持つコア。
fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create docs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx("tenant-a"), s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match run(core, s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(c) => c,
    }
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn select_docs_rows(core: &EngineCore, s: &mut SessionState) -> Result<usize, String> {
    match run(
        core,
        s,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10",
    )? {
        SqlOutcome::Query(r) => Ok(r.rows.len()),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn all_prefix_forms_create_without_ddl_permission() {
    let (core, path) = new_core("t44-ddl-forms");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    for (i, prefix) in [
        "CREATE TEMP TABLE",
        "CREATE TEMPORARY TABLE",
        "CREATE LOCAL TEMP TABLE",
        "CREATE LOCAL TEMPORARY TABLE",
        "CREATE GLOBAL TEMP TABLE",
        "CREATE GLOBAL TEMPORARY TABLE",
    ]
    .iter()
    .enumerate()
    {
        let out = run(&core, &mut s, &format!("{prefix} t{i} (a TEXT)")).expect(prefix);
        assert!(matches!(out, SqlOutcome::CreateTable(_)), "{prefix}");
    }
    // 永続表の DDL は従来どおり権限が要る。
    assert_eq!(code(&core, &mut s, "CREATE TABLE p (a TEXT)"), "42501");
    assert_eq!(code(&core, &mut s, "DROP TABLE docs"), "42501");
}

#[test]
fn preserve_rows_is_the_default_and_names_resolve_to_the_temp_table() {
    let (core, path) = new_core("t44-preserve");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    run(&core, &mut s, "CREATE TEMP TABLE t (a TEXT)").unwrap();
    assert_eq!(code(&core, &mut s, "CREATE TEMP TABLE t (a TEXT)"), "42P07");
    assert!(matches!(
        run(&core, &mut s, "CREATE TEMP TABLE IF NOT EXISTS t (a TEXT)"),
        Ok(SqlOutcome::CreateTable(_))
    ));
    run(
        &core,
        &mut s,
        "CREATE TEMP TABLE t2 (a TEXT) ON COMMIT PRESERVE ROWS",
    )
    .unwrap();
    // 明示の DELETE ROWS でも表は残る。
    run(
        &core,
        &mut s,
        "CREATE TEMP TABLE t3 (a TEXT) ON COMMIT DELETE ROWS",
    )
    .unwrap();
    assert_eq!(
        code(&core, &mut s, "CREATE TEMP TABLE t3 (a TEXT)"),
        "42P07"
    );
}

#[test]
fn on_commit_drop_removes_the_table_at_the_end_of_the_statement() {
    let (core, path) = new_core("t44-drop");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    run(&core, &mut s, "CREATE TEMP TABLE d (a TEXT) ON COMMIT DROP").unwrap();
    // 自動コミットの文の終わりで消えているので、同名を再び作れる。
    run(&core, &mut s, "CREATE TEMP TABLE d (a TEXT) ON COMMIT DROP").unwrap();
    // 消えた名前の DROP は一時表でないので永続側の経路（権限ゲート）へ回る。
    assert_eq!(code(&core, &mut s, "DROP TABLE d"), "42501");
    let mut ddl = ddl_session();
    run(
        &core,
        &mut ddl,
        "CREATE TEMP TABLE d (a TEXT) ON COMMIT DROP",
    )
    .unwrap();
    assert_eq!(code(&core, &mut ddl, "DROP TABLE d"), "42P01");
}

#[test]
fn temp_tables_coexist_with_a_persistent_table_of_the_same_name() {
    let (core, path) = new_core("t44-same-name");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    run(&core, &mut s, "CREATE TEMP TABLE docs (a TEXT)").unwrap();
    // IF NOT EXISTS でも、同名の永続表ではなく一時スキーマだけを見て作る。
    let mut s2 = SessionState::default();
    run(
        &core,
        &mut s2,
        "CREATE TEMP TABLE IF NOT EXISTS docs (a TEXT)",
    )
    .unwrap();
    assert_eq!(
        code(&core, &mut s2, "CREATE TEMP TABLE docs (a TEXT)"),
        "42P07"
    );

    // 別セッションには見えず、永続表がそのまま読める。
    let mut other = SessionState::default();
    assert_eq!(select_docs_rows(&core, &mut other), Ok(0));

    // 一時表を DROP すると、永続表は無傷のまま名前が永続側へ戻る。
    run(&core, &mut s, "DROP TABLE docs").unwrap();
    assert_eq!(select_docs_rows(&core, &mut s), Ok(0));
    let mut ddl = ddl_session();
    assert!(run(&core, &mut ddl, "DROP TABLE docs").is_ok());
}

#[test]
fn drop_table_routes_between_temp_and_persistent_tables() {
    let (core, path) = new_core("t44-drop-route");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    run(&core, &mut s, "CREATE TEMP TABLE a (x TEXT)").unwrap();
    run(&core, &mut s, "CREATE TEMP TABLE b (x TEXT)").unwrap();

    // 一時表だけのリスト（CASCADE／RESTRICT も受理）。
    assert!(matches!(
        run(&core, &mut s, "DROP TABLE a, b CASCADE"),
        Ok(SqlOutcome::DropTable(_))
    ));
    run(&core, &mut s, "CREATE TEMP TABLE a (x TEXT)").unwrap();
    assert!(run(&core, &mut s, "DROP TABLE IF EXISTS a RESTRICT").is_ok());
    // もう一時表ではないので IF EXISTS は永続側の no-op になる。
    assert!(run(&core, &mut s, "DROP TABLE IF EXISTS a").is_ok());

    // 一時表と永続表の混在は 0A000（状態は変えない）。
    run(&core, &mut s, "CREATE TEMP TABLE a (x TEXT)").unwrap();
    assert_eq!(code(&core, &mut s, "DROP TABLE a, docs"), "0A000");
    assert_eq!(code(&core, &mut s, "DROP TABLE a, nope"), "0A000");
    assert_eq!(code(&core, &mut s, "CREATE TEMP TABLE a (x TEXT)"), "42P07");
    assert_eq!(select_docs_rows(&core, &mut s), Ok(0));

    // 永続表だけのリストは従来どおり（DDL 権限あり）。
    assert!(run(&core, &mut s, "DROP TABLE docs").is_ok());
}

#[test]
fn unsupported_forms_are_rejected_fail_closed() {
    let (core, path) = new_core("t44-unsupported");
    let _g = CleanupGuard(path);
    let mut s = SessionState::default();
    // VECTOR・ENUM・CTAS は 0A000。
    for sql in [
        "CREATE TEMP TABLE v (e VECTOR(3))",
        "CREATE TEMP TABLE v AS SELECT 1",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    let mut ddl = ddl_session();
    run(&core, &mut ddl, "CREATE TYPE mood AS ENUM ('ok', 'ng')").unwrap();
    assert_eq!(
        code(&core, &mut ddl, "CREATE TEMP TABLE v (c mood)"),
        "0A000"
    );
    // 権限の無いセッションに対しても型の存否を観測させない（同じ 0A000）。
    assert_eq!(
        code(&core, &mut s, "CREATE TEMP TABLE v (c nomood)"),
        "0A000"
    );
    // 素の CREATE TABLE の ON COMMIT は 42P16。
    assert_eq!(
        code(&core, &mut ddl, "CREATE TABLE p (a TEXT) ON COMMIT DROP"),
        "42P16"
    );
    // 拒否された文は何も登録しない。
    run(&core, &mut s, "CREATE TEMP TABLE v (a TEXT)").unwrap();
}

#[test]
fn foreign_keys_involving_temp_tables_are_rejected() {
    let (core, path) = new_core("t44-fk");
    let _g = CleanupGuard(path);
    let mut s = ddl_session();
    run(&core, &mut s, "CREATE TABLE parent (k TEXT PRIMARY KEY)").unwrap();
    // 一時表から永続表への FK は 42P16。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE c (a TEXT, FOREIGN KEY (a) REFERENCES parent (k))"
        ),
        "42P16"
    );
    // 一時表どうしの FK は実行段が未実装のため 0A000。
    run(&core, &mut s, "CREATE TEMP TABLE tp (k TEXT PRIMARY KEY)").unwrap();
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TEMP TABLE c (a TEXT, FOREIGN KEY (a) REFERENCES tp (k))"
        ),
        "0A000"
    );
    // 永続表から、このセッションの一時表の名前への FK は 42P16（同名の永続表を黙って参照しない）。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE pc (a TEXT, FOREIGN KEY (a) REFERENCES tp (k))"
        ),
        "42P16"
    );
    // IF NOT EXISTS ＋既存の永続表なら、一時表への FK を含む定義でも検証より先に no-op 成功
    // （既存名の判定が FK 検証に優先する）。実際に新規作成する場合だけ 42P16。
    run(&core, &mut s, "CREATE TABLE p (a TEXT)").unwrap();
    run(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS p (a TEXT, FOREIGN KEY (a) REFERENCES tp (k))",
    )
    .unwrap();
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE IF NOT EXISTS pn (a TEXT, FOREIGN KEY (a) REFERENCES tp (k))"
        ),
        "42P16"
    );
    // 一時表が無い別セッションでは、通常どおり名前解決の結果（存在しない）になる。
    let mut other = ddl_session();
    assert_eq!(
        code(
            &core,
            &mut other,
            "CREATE TABLE pc (a TEXT, FOREIGN KEY (a) REFERENCES tp (k))"
        ),
        "42P01"
    );
}

#[test]
fn explicit_transactions_reject_temp_ddl_like_other_ddl() {
    let (core, path) = new_core("t44-txn");
    let _g = CleanupGuard(path);
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    run(&core, &mut s, "CREATE TEMP TABLE t (a TEXT)").unwrap();
    for sql in ["CREATE TEMP TABLE u (a TEXT)", "DROP TABLE t"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
            .unwrap();
        let err = core
            .execute_sql_in_txn(&c, &mut s, &mut txn, sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
            .unwrap();
    }
    // どちらも状態を変えていない。
    assert_eq!(code(&core, &mut s, "CREATE TEMP TABLE t (a TEXT)"), "42P07");
    run(&core, &mut s, "CREATE TEMP TABLE u (a TEXT)").unwrap();
}

#[test]
fn session_limits_cap_the_number_of_temp_tables() {
    let path = unique_db_path("t44-limits");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_temp_table_limits(TempTableLimits::new(2, 1024 * 1024));
    let mut s = SessionState::default();
    run(&core, &mut s, "CREATE TEMP TABLE a (x TEXT)").unwrap();
    run(&core, &mut s, "CREATE TEMP TABLE b (x TEXT)").unwrap();
    assert_eq!(code(&core, &mut s, "CREATE TEMP TABLE c (x TEXT)"), "54000");
    // 拒否された CREATE は何も変えない。DROP で空きができれば作れる。
    assert_eq!(code(&core, &mut s, "CREATE TEMP TABLE c (x TEXT)"), "54000");
    run(&core, &mut s, "DROP TABLE a").unwrap();
    run(&core, &mut s, "CREATE TEMP TABLE c (x TEXT)").unwrap();
    // 上限はセッション単位。別セッションは独立に作れる。
    let mut other = SessionState::default();
    run(&core, &mut other, "CREATE TEMP TABLE a (x TEXT)").unwrap();
    // IF NOT EXISTS の既存一時表は上限に達していても no-op 成功。
    assert!(run(&core, &mut s, "CREATE TEMP TABLE IF NOT EXISTS b (x TEXT)").is_ok());
    assert_eq!(core.temp_table_limits().max_tables_per_session(), 2);
}

#[test]
fn parsed_statements_pass_the_coarse_gate_for_temp_ddl() {
    let (core, path) = new_core("t44-parsed");
    let _g = CleanupGuard(path);
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    // 拡張クエリ経路（parse 済みを実行）。一時表を作った後も 2 個目の CREATE TEMP と DROP が通る。
    for sql in [
        "CREATE TEMP TABLE a (x TEXT)",
        "CREATE TEMP TABLE b (x TEXT) ON COMMIT DROP",
        "CREATE TEMP TABLE c (x TEXT)",
        "DROP TABLE a",
    ] {
        let parsed = core.parse_sql(sql).expect(sql);
        core.execute_parsed_in_session(&c, &mut s, &parsed)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let parsed = core.parse_sql("CREATE TEMP TABLE a (x TEXT)").unwrap();
    core.execute_parsed_in_session(&c, &mut s, &parsed)
        .expect("a was dropped");
    // セッションを持たない parse で作られた文は、一時カタログが空でないセッションでは粗いゲートで
    // 0A000 のまま（名前が一時表かを判定できないため。セッション対応の parse は #1684）。
    let sel = core
        .parse_sql("SELECT id FROM docs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 1")
        .unwrap();
    let err = core
        .execute_parsed_in_session(&c, &mut s, &sel)
        .expect_err("gate");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn another_session_never_sees_the_temp_table() {
    let (core, path) = new_core("t44-isolation");
    let _g = CleanupGuard(path);
    let mut a = SessionState::default();
    run(&core, &mut a, "CREATE TEMP TABLE only_a (x TEXT)").unwrap();
    // 別セッションは同名を作れて、永続側の DROP は 42501（一時表の存在を示さない）。
    let mut b = SessionState::default();
    assert_eq!(code(&core, &mut b, "DROP TABLE only_a"), "42501");
    run(&core, &mut b, "CREATE TEMP TABLE only_a (x TEXT)").unwrap();
    // 別テナントの文脈でも自セッションの一時表は自セッションだけのもの。
    let mut c = SessionState::default();
    assert!(core
        .execute_sql_in_session(
            &ctx("tenant-b"),
            &mut c,
            "CREATE TEMP TABLE only_a (x TEXT)"
        )
        .is_ok());
}
