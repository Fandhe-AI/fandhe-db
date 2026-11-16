//! `UPDATE ... FROM` の SET の行値代入と FROM 側列を含む式の SQL 経由の受け入れテスト
//! （SQL-53 (b)・SQL-28・RLS-10・RECOVER-10/11・Issue #3296。親 #3290）。
//!
//! 契約: `SET (a, b) = (s.p, s.q)`／`= ROW(...)`・`SET a = s.p + 1`（修飾あり・なし）が受理され、値は
//! 単一表の SET と同じ型検査・暗黙キャストで決まる。列数の不一致（`42601`）・型の不整合（`42804`）・
//! 未知列／修飾子は拒否され、拒否された文は行も台帳も変えない。source の走査は書き込むセッションの
//! `PolicyContext` で行うため、他テナントの行は値の材料にも対象にもならない。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// t(k, a, b, n)、s(k, p, q, big)。`a`／`b`／`p`／`q` は INTEGER、`n` は SMALLINT、`big` は BIGINT。
fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let int = |n: &str| ColumnDef::new(n, ColumnType::Integer, true);
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                int("a"),
                int("b"),
                ColumnDef::new("n", ColumnType::SmallInt, true),
            ],
        ))
        .expect("create t");
    storage
        .create_table(&TableSchema::new(
            "s",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                int("p"),
                int("q"),
                ColumnDef::new("big", ColumnType::BigInt, true),
            ],
        ))
        .expect("create s");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn code(r: Result<String, SqlSurfaceError>) -> String {
    r.expect_err("must be rejected").wire_code().to_string()
}

fn upd(n: u64) -> String {
    format!("Update(UpdateOutcome {{ rows_affected: {n} }})")
}

fn rows_t(core: &EngineCore, ctx: &PolicyContext) -> String {
    run(
        core,
        ctx,
        "SELECT id, k, a, b, n FROM t ORDER BY id LIMIT 100",
    )
    .expect("scan t")
}

/// t: (1, 'x', a=1, b=2, n=3)。s: (1, 'x', p=1010, q=2020, big=3030)。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    run(
        core,
        ctx,
        "INSERT INTO t (id, embedding, k, a, b, n) VALUES (1, '[0.1,0.2]', 'x', 1, 2, 3)",
    )
    .expect("insert t");
    run(
        core,
        ctx,
        "INSERT INTO s (id, embedding, k, p, q, big) VALUES (1, '[0.1,0.2]', 'x', 1010, 2020, 3030)",
    )
    .expect("insert s");
}

const WHERE: &str = "FROM s WHERE t.k = s.k";

/// 1 文を実行し、更新後の `t` に `expect` が含まれ `absent` が含まれないこと（と影響行数 1）を確かめる。
fn assert_updates(sql: &str, expect: &[&str], absent: &[&str]) {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-rowexpr");
    seed(&core, &alice);
    assert_eq!(
        run(&core, &alice, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")),
        upd(1)
    );
    let rows = rows_t(&core, &alice);
    for e in expect {
        assert!(rows.contains(e), "{sql}: {rows}");
    }
    for e in absent {
        assert!(!rows.contains(e), "{sql}: {rows}");
    }
}

/// 行値代入（`ROW` あり・なし）が source の列を各代入先へ運ぶ。
#[test]
fn row_value_assignment_takes_source_columns() {
    for form in ["(s.p, s.q)", "ROW(s.p, s.q)", "(p, q)"] {
        assert_updates(
            &format!("UPDATE t SET (a, b) = {form} {WHERE}"),
            &["1010", "2020"],
            &[],
        );
    }
}

/// FROM 側の列を含む式（修飾あり・なし・対象列との混在）。
#[test]
fn expressions_over_source_columns() {
    assert_updates(&format!("UPDATE t SET a = s.p + 1 {WHERE}"), &["1011"], &[]);
    assert_updates(&format!("UPDATE t SET a = p + 1 {WHERE}"), &["1011"], &[]);
    assert_updates(
        &format!("UPDATE t SET a = t.a + s.p {WHERE}"),
        &["1011"],
        &[],
    );
    assert_updates(
        &format!("UPDATE t SET (a, b) = (s.p + 1, s.q * 2 + t.b) {WHERE}"),
        &["1011", "4042"],
        &[],
    );
    // 行内の `DEFAULT` との混在（`b` に既定値は無く NULL になる）。
    assert_updates(
        &format!("UPDATE t SET (a, b) = (s.p + 1, DEFAULT) {WHERE}"),
        &["1011"],
        &["2020"],
    );
}

/// 入れ替え `(a, b) = (b, a)` は更新前の値を使う。
#[test]
fn swap_uses_pre_update_values() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-swap");
    seed(&core, &alice);
    assert_eq!(
        run(
            &core,
            &alice,
            &format!("UPDATE t SET (a, b) = (b, a) {WHERE}")
        )
        .expect("swap"),
        upd(1)
    );
    let rows = rows_t(&core, &alice);
    let (core2, _g2) = new_core("sql53-set-swap-ref");
    seed(&core2, &alice);
    run(&core2, &alice, "UPDATE t SET a = 2, b = 1 WHERE k = 'x'").expect("reference");
    assert_eq!(rows, rows_t(&core2, &alice));
}

/// 暗黙キャストは単一表の SET と同じ: BIGINT 由来の式を INTEGER 列へ、範囲外は `22003`。
#[test]
fn implicit_casts_follow_single_table_set() {
    assert_updates(
        &format!("UPDATE t SET a = s.big + 1 {WHERE}"),
        &["3031"],
        &[],
    );
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-cast");
    seed(&core, &alice);
    run(
        &core,
        &alice,
        "UPDATE s SET big = 99999999999 WHERE k = 'x'",
    )
    .expect("big");
    let before = rows_t(&core, &alice);
    assert_eq!(
        code(run(
            &core,
            &alice,
            &format!("UPDATE t SET a = s.big + 1 {WHERE}")
        )),
        "22003"
    );
    assert_eq!(before, rows_t(&core, &alice));
}

/// 誤った形は固定の SQLSTATE で拒否され、行が不変で台帳も消費しない。
#[test]
fn malformed_forms_are_rejected_without_side_effects() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-reject");
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for (sql, want) in [
        // 列数の不一致
        (format!("UPDATE t SET (a, b) = (s.p) {WHERE}"), "42601"),
        (
            format!("UPDATE t SET (a, b) = (s.p, s.q, 1) {WHERE}"),
            "42601",
        ),
        // 1 列の括弧形は ROW 必須
        (format!("UPDATE t SET (a) = (s.p) {WHERE}"), "42601"),
        // サブクエリ源
        (
            format!("UPDATE t SET (a, b) = (SELECT 1, 2) {WHERE}"),
            "0A000",
        ),
        // 型の不整合（TEXT 列へ数値式・INTEGER 列へ TEXT）
        (format!("UPDATE t SET k = s.p + 1 {WHERE}"), "42804"),
        (format!("UPDATE t SET (a, b) = (s.p, s.k) {WHERE}"), "42804"),
        // 未知列・未知の修飾子・疑似列 id（source 側は運べない）
        (format!("UPDATE t SET a = s.nope + 1 {WHERE}"), "42703"),
        (format!("UPDATE t SET a = zz.p + 1 {WHERE}"), "42P01"),
        (format!("UPDATE t SET a = s.id + 1 {WHERE}"), "0A000"),
        // 両表に存在する非修飾名は曖昧
        (format!("UPDATE t SET a = length(k) {WHERE}"), "42702"),
    ] {
        assert_eq!(code(run(&core, &alice, &sql)), want, "{sql}");
        if want != "42601" {
            // 拒否された文は台帳を消費しない。
            let with_op = format!("{sql} USING OPERATION_ID 'rej-1'");
            assert_ne!(code(run(&core, &alice, &with_op)), "23505", "{sql}");
        }
    }
    assert_eq!(before, rows_t(&core, &alice));
    assert_eq!(
        run(
            &core,
            &alice,
            &format!("UPDATE t SET a = s.p + 1 {WHERE} USING OPERATION_ID 'rej-1'")
        )
        .expect("op id still unused"),
        upd(1)
    );
}

/// 他テナントの source 行は値の材料にならず、他テナントの対象行も更新されない（件数も漏れない）。
#[test]
fn other_tenants_rows_never_contribute() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-set-rls");
    // alice の対象行に対し、同じキーの source は bob にしか無い。
    run(
        &core,
        &alice,
        "INSERT INTO t (id, embedding, k, a, b, n) VALUES (1, '[0.1,0.2]', 'x', 1, 2, 3)",
    )
    .expect("alice t");
    run(
        &core,
        &bob,
        "INSERT INTO s (id, embedding, k, p, q, big) VALUES (1, '[0.1,0.2]', 'x', 7777, 8888, 9999)",
    )
    .expect("bob s");
    let before = rows_t(&core, &alice);
    for sql in [
        format!("UPDATE t SET (a, b) = (s.p, s.q) {WHERE}"),
        format!("UPDATE t SET a = s.p + 1 {WHERE}"),
    ] {
        assert_eq!(run(&core, &alice, &sql).expect("no match"), upd(0), "{sql}");
    }
    assert_eq!(before, rows_t(&core, &alice));
    // bob は自分の source だけで自分の対象行を更新し、alice の行には触れない。
    run(
        &core,
        &bob,
        "INSERT INTO t (id, embedding, k, a, b, n) VALUES (2, '[0.1,0.2]', 'x', 5, 6, 7)",
    )
    .expect("bob t");
    assert_eq!(
        run(&core, &bob, &format!("UPDATE t SET a = s.p + 1 {WHERE}")).expect("bob"),
        upd(1)
    );
    assert_eq!(before, rows_t(&core, &alice));
}

/// 明示 Tx: COMMIT で確定し、ROLLBACK で破棄される。
#[test]
fn explicit_transaction_commit_and_rollback() {
    let alice = ctx("alice");
    for (finish, changed) in [("COMMIT", true), ("ROLLBACK", false)] {
        let (core, _g) = new_core("sql53-set-txn");
        seed(&core, &alice);
        let before = rows_t(&core, &alice);
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let out = core
            .execute_sql_in_txn(
                &alice,
                &mut session,
                &mut txn,
                &format!("UPDATE t SET (a, b) = (s.p + 1, s.q) {WHERE}"),
            )
            .expect("update");
        assert_eq!(format!("{out:?}"), upd(1));
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, finish)
            .expect("finish");
        assert_eq!(before != rows_t(&core, &alice), changed, "{finish}");
    }
}

/// 台帳: 再送は `23505`、別文への再利用は `22023`、二重適用されない。
#[test]
fn operation_id_resend_and_reuse() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-ledger");
    seed(&core, &alice);
    let sql = format!("UPDATE t SET a = s.p + 1 {WHERE} USING OPERATION_ID 'set-1'");
    assert_eq!(run(&core, &alice, &sql).expect("first"), upd(1));
    run(&core, &alice, "UPDATE s SET p = 5000 WHERE k = 'x'").expect("change source");
    let after_first = rows_t(&core, &alice);
    assert_eq!(code(run(&core, &alice, &sql)), "23505");
    assert_eq!(after_first, rows_t(&core, &alice));
    let other = format!("UPDATE t SET b = s.q + 1 {WHERE} USING OPERATION_ID 'set-1'");
    assert_eq!(code(run(&core, &alice, &other)), "22023");
    assert_eq!(after_first, rows_t(&core, &alice));
}

/// Describe は Execute と同じ準備段を通し、同じ拒否コードを返す。EXPLAIN は新しい形でも計画を返し、
/// 行・台帳に触れない。
#[test]
fn describe_and_explain_cover_the_new_forms() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-describe");
    seed(&core, &alice);
    let session = SessionState::default();
    for sql in [
        format!("UPDATE t SET (a, b) = (s.p, s.q) {WHERE}"),
        format!("UPDATE t SET a = s.p + 1 {WHERE}"),
    ] {
        let parsed = core.parse_sql(&sql).expect("parse");
        assert!(core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .is_none());
    }
    for (sql, want) in [
        (format!("UPDATE t SET k = s.p + 1 {WHERE}"), "42804"),
        (format!("UPDATE t SET a = s.nope + 1 {WHERE}"), "42703"),
    ] {
        let parsed = core.parse_sql(&sql).expect("parse");
        let d = core
            .describe_parsed_in_session(&session, &parsed)
            .expect_err("describe")
            .wire_code()
            .to_string();
        assert_eq!(d, want, "{sql}");
        assert_eq!(code(run(&core, &alice, &sql)), want, "{sql}");
    }
    let before = rows_t(&core, &alice);
    let out = run(
        &core,
        &alice,
        &format!("EXPLAIN UPDATE t SET a = s.p + 1 {WHERE} USING OPERATION_ID 'ex-1'"),
    )
    .expect("explain");
    assert!(out.starts_with("Explain"), "{out}");
    assert_eq!(before, rows_t(&core, &alice));
    assert_eq!(
        run(
            &core,
            &alice,
            &format!("UPDATE t SET a = s.p + 1 {WHERE} USING OPERATION_ID 'ex-1'")
        )
        .expect("ledger untouched"),
        upd(1)
    );
}

/// 末尾 `;` 付きの単独送信（psql 形）は `;` なしと同じ結果になる。
#[test]
fn trailing_semicolon_is_accepted() {
    assert_updates(
        &format!("UPDATE t SET (a, b) = (s.p + 1, s.q) {WHERE};"),
        &["1011", "2020"],
        &[],
    );
}

/// 単純形ビューを source にしても式・行値代入が評価される（Issue #3292 の経路）。
#[test]
fn simple_view_source_supports_expressions() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-set-view");
    seed(&core, &alice);
    let mut ddl_session = SessionState::default();
    ddl_session.allow_ddl();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE VIEW sv AS SELECT k, p, q FROM s",
    )
    .expect("view");
    assert_eq!(
        run(
            &core,
            &alice,
            "UPDATE t SET (a, b) = (sv.p + 1, sv.q) FROM sv WHERE t.k = sv.k"
        )
        .expect("update"),
        upd(1)
    );
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("1011") && rows.contains("2020"), "{rows}");
}
