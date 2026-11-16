//! `UPDATE ... FROM`／`DELETE ... USING` の SQL 経由の受け入れテスト（SQL-53 (b)・RLS-10・RECOVER-10/11・Issue #3205。親 #3200）。
//!
//! 役割: dispatch の最小固定（`sql53_dml_from_using_exec.rs`）・束縛／変換器の拒否表と台帳の基本
//! （`sql53_dml_from_using_bind.rs`）の上に、結合形 DML の SQL 経路を 6 観点（多重一致・RLS・上限・
//! 再送冪等・明示 Tx・自己結合）で体系的に固定する。あわせて未対応の形が副作用ゼロ（行・台帳が不変）で
//! fail-closed になることを確認する。上限は `EngineCore` のビルダーで渡すだけで環境変数・共有状態は使わない。
//! ポインタ: `docs/design/update-from-delete-using.md`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::scan_limits::ScanLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn build(label: &str, tune: impl FnOnce(EngineCore) -> EngineCore) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("x", ColumnType::Integer, true),
            ],
        ))
        .expect("create t");
    storage
        .create_table(&TableSchema::new(
            "s",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("v", ColumnType::Integer, true),
            ],
        ))
        .expect("create s");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (tune(core), CleanupGuard(path))
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    build(label, |c| c)
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

fn insert(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, k: &str, n: i64) {
    let col = if table == "t" { "x" } else { "v" };
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {n})"
        ),
    )
    .expect("insert");
}

/// 呼び出し元から見える `t` の `(id, k, x)` を id 昇順で返す（Debug 文字列の比較用）。
fn rows_t(core: &EngineCore, ctx: &PolicyContext) -> String {
    run(core, ctx, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t")
}

/// t: (1,a,10) (2,b,20) (3,c,30)。s: (1,a,100) (2,b,200)。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        insert(core, ctx, "t", id, k, x);
    }
    for (id, k, v) in [(1, "a", 100), (2, "b", 200)] {
        insert(core, ctx, "s", id, k, v);
    }
}

const UPD: &str = "UPDATE t SET x = s.v FROM s WHERE t.k = s.k";
const DEL: &str = "DELETE FROM t USING s WHERE t.k = s.k";

fn tag(kind: &str, n: u64) -> String {
    match kind {
        "UPD" => format!("Update(UpdateOutcome {{ rows_affected: {n} }})"),
        _ => format!("Delete(DeleteOutcome {{ rows_affected: {n} }})"),
    }
}

/// 明示 Tx で 1 文だけ実行し、結果（または拒否コード）を返してから `finish`（COMMIT／ROLLBACK）で終える。
fn explicit_one(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
    finish: &str,
) -> Result<String, String> {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(ctx, &mut session, &mut txn, sql)
        .map(|o| format!("{o:?}"))
        .map_err(|e| e.wire_code().to_string());
    // 失敗した Tx でも ROLLBACK は受理される。成功した Tx は指定どおり終える。
    core.execute_sql_in_txn(ctx, &mut session, &mut txn, finish)
        .expect("finish");
    out
}

/// autocommit か明示 Tx（COMMIT）で 1 文を実行する。両経路で同じ契約を確かめるための入口。
fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str, explicit: bool) -> String {
    if explicit {
        explicit_one(core, ctx, sql, "COMMIT").expect(sql)
    } else {
        run(core, ctx, sql).expect(sql)
    }
}

fn dml_limit(n: usize) -> DmlLimits {
    DmlLimits {
        max_affected_rows: std::num::NonZeroUsize::new(n),
        max_insert_rows_per_statement: None,
    }
}

// ---- 1. 多重一致 ----

/// 多重一致: 対象 1 行に source 3 行が一致しても影響行数は対象行数（一意化後）で、UPDATE は走査順で最初の
/// source 行の値を採る。DELETE も対象行を 1 回だけ消す。autocommit と明示 Tx で一致する。
#[test]
fn many_source_rows_per_target_count_each_target_once() {
    let alice = ctx("alice");
    for explicit in [false, true] {
        let (core, _g) = new_core("sql53-acc-multi-upd");
        seed(&core, &alice);
        insert(&core, &alice, "s", 3, "a", 901);
        insert(&core, &alice, "s", 4, "a", 902);
        assert_eq!(exec(&core, &alice, UPD, explicit), tag("UPD", 2));
        let rows = rows_t(&core, &alice);
        assert!(rows.contains("100") && rows.contains("200"), "{rows}");
        assert!(!rows.contains("901") && !rows.contains("902"), "{rows}");

        let (core, _g) = new_core("sql53-acc-multi-del");
        seed(&core, &alice);
        insert(&core, &alice, "s", 3, "a", 901);
        insert(&core, &alice, "s", 4, "a", 902);
        assert_eq!(exec(&core, &alice, DEL, explicit), tag("DEL", 2));
        let rows = rows_t(&core, &alice);
        assert!(rows.contains("30") && !rows.contains("10"), "{rows}");
    }
}

/// 対象側・source 側の述語（JOIN WHERE のリテラルは文字列のみ対応）を併用しても、多重一致は対象行ごとに 1 回で数える。
#[test]
fn per_side_predicates_filter_matches_before_deduplication() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-acc-pred");
    seed(&core, &alice);
    insert(&core, &alice, "s", 3, "a", 901);
    let sql = "UPDATE t SET x = s.v FROM s WHERE t.k = s.k AND t.k = 'a' AND s.k = 'a'";
    assert_eq!(run(&core, &alice, sql).expect("update"), tag("UPD", 1));
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("100") && !rows.contains("200"), "{rows}");
    let sql = "DELETE FROM t USING s WHERE t.k = s.k AND t.k = 'b' AND s.k = 'b'";
    assert_eq!(run(&core, &alice, sql).expect("delete"), tag("DEL", 1));
}

/// 一致が 0 件でも成功し、影響行数 0・行は不変。
#[test]
fn zero_matches_is_a_successful_noop() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-acc-zero");
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for explicit in [false, true] {
        assert_eq!(
            exec(
                &core,
                &alice,
                "UPDATE t SET x = 1 FROM s WHERE t.k = s.k AND t.k = 'zzz'",
                explicit
            ),
            tag("UPD", 0)
        );
        assert_eq!(
            exec(
                &core,
                &alice,
                "DELETE FROM t USING s WHERE t.k = s.k AND t.k = 'zzz'",
                explicit
            ),
            tag("DEL", 0)
        );
    }
    assert_eq!(before, rows_t(&core, &alice));
}

// ---- 2. RLS ----

/// RLS-10: 他テナントの Private な source 行は一致せず、他テナントの対象行は変更・削除されない。
/// 影響行数に他テナントの存在・件数は現れない（他テナントの行を持つ・持たないで結果が同一）。
#[test]
fn other_tenant_private_rows_never_match_nor_leak_counts() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    for explicit in [false, true] {
        let (clean, _g1) = new_core("sql53-acc-rls-clean");
        seed(&clean, &alice);
        let (noisy, _g2) = new_core("sql53-acc-rls-noisy");
        seed(&noisy, &alice);
        insert(&noisy, &bob, "s", 10, "c", 777);
        insert(&noisy, &bob, "s", 11, "a", 778);
        insert(&noisy, &bob, "t", 20, "a", 5);
        insert(&noisy, &bob, "t", 21, "b", 6);
        let a = exec(&clean, &alice, UPD, explicit);
        let b = exec(&noisy, &alice, UPD, explicit);
        assert_eq!(a, b, "affected count must not depend on other tenants");
        assert_eq!(a, tag("UPD", 2));
        assert_eq!(rows_t(&clean, &alice), rows_t(&noisy, &alice));
        assert!(!rows_t(&noisy, &alice).contains("777"));
        let bob_rows = rows_t(&noisy, &bob);
        assert!(
            bob_rows.contains('5') && bob_rows.contains('6'),
            "{bob_rows}"
        );
        assert!(!bob_rows.contains("100"), "{bob_rows}");

        assert_eq!(exec(&clean, &alice, DEL, explicit), tag("DEL", 2));
        assert_eq!(exec(&noisy, &alice, DEL, explicit), tag("DEL", 2));
        let bob_rows = rows_t(&noisy, &bob);
        assert!(
            bob_rows.contains('5') && bob_rows.contains('6'),
            "bob's rows must remain"
        );
    }
}

/// 他テナントしか source を持たない場合、一致は 0 件（成功の no-op）で他テナントの存在はエラーにも現れない。
#[test]
fn source_owned_only_by_another_tenant_matches_nothing() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-acc-rls-only-other");
    for (id, k, x) in [(1, "a", 10), (2, "b", 20)] {
        insert(&core, &alice, "t", id, k, x);
    }
    insert(&core, &bob, "s", 1, "a", 100);
    let before = rows_t(&core, &alice);
    assert_eq!(run(&core, &alice, UPD).expect("update"), tag("UPD", 0));
    assert_eq!(run(&core, &alice, DEL).expect("delete"), tag("DEL", 0));
    assert_eq!(before, rows_t(&core, &alice));
}

// ---- 3. 上限 ----

/// 影響行数の上限（一意化後の対象行数）: ちょうどは成功し、超過は `54000` で行が不変。
#[test]
fn dml_affected_rows_limit_is_exact_and_leaves_no_side_effects() {
    let alice = ctx("alice");
    for explicit in [false, true] {
        // 多重一致で source は 3 行でも、対象行は 2 行（上限 2 は成功）。
        let (core, _g) = build("sql53-acc-lim-dml-ok", |c| c.with_dml_limits(dml_limit(2)));
        seed(&core, &alice);
        insert(&core, &alice, "s", 3, "a", 901);
        assert_eq!(exec(&core, &alice, UPD, explicit), tag("UPD", 2));

        let (core, _g) = build("sql53-acc-lim-dml-over", |c| {
            c.with_dml_limits(dml_limit(1))
        });
        seed(&core, &alice);
        let before = rows_t(&core, &alice);
        for sql in [UPD, DEL] {
            let r = if explicit {
                explicit_one(&core, &alice, sql, "ROLLBACK")
            } else {
                run(&core, &alice, sql).map_err(|e| e.wire_code().to_string())
            };
            assert_eq!(r.expect_err(sql), "54000", "{sql}");
            assert_eq!(before, rows_t(&core, &alice), "{sql}");
        }
    }
}

/// 結合の入力行数・出力行数の上限: 超過は `54000` で副作用ゼロ。上限ちょうどは成功する。
#[test]
fn join_size_limits_reject_over_limit_without_side_effects() {
    let alice = ctx("alice");
    let (core, _g) = build("sql53-acc-lim-join-out", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 1))
    });
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for sql in [UPD, DEL] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, rows_t(&core, &alice), "{sql}");
    }
    let (core, _g) = build("sql53-acc-lim-join-out-ok", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 100_000, 2))
    });
    seed(&core, &alice);
    assert_eq!(run(&core, &alice, UPD).expect("update"), tag("UPD", 2));

    let (core, _g) = build("sql53-acc-lim-join-in", |c| {
        c.with_join_limits(JoinSizeLimits::new(2, 1, 100_000))
    });
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    for sql in [UPD, DEL] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
        assert_eq!(before, rows_t(&core, &alice), "{sql}");
    }
}

/// 総走査行数の上限: 超過は `54000`。上限を緩めた構成では同じ文が成功する。
#[test]
fn scan_limit_rejects_over_limit() {
    let alice = ctx("alice");
    let (core, _g) = build("sql53-acc-lim-scan", |c| {
        c.with_scan_limits(ScanLimits::new(1))
    });
    seed(&core, &alice);
    for sql in [UPD, DEL] {
        assert_eq!(code(run(&core, &alice, sql)), "54000", "{sql}");
    }
    let (open, _g2) = new_core("sql53-acc-lim-scan-open");
    seed(&open, &alice);
    assert_eq!(run(&open, &alice, UPD).expect("update"), tag("UPD", 2));
}

// ---- 4. 再送冪等 ----

fn op(sql: &str, id: &str) -> String {
    format!("{sql} USING OPERATION_ID '{id}'")
}

/// 再送は `23505` で二重適用されず、別文への再利用は `22023`。
#[test]
fn resend_is_rejected_and_never_double_applies() {
    let alice = ctx("alice");
    for (sql, kind) in [(UPD, "UPD"), (DEL, "DEL")] {
        let (core, _g) = new_core("sql53-acc-resend");
        seed(&core, &alice);
        let with_op = op(sql, "acc-1");
        assert_eq!(run(&core, &alice, &with_op).expect("first"), tag(kind, 2));
        // 再適用されるなら影響を受ける行（c）を足してから再送する。
        insert(&core, &alice, "s", 9, "c", 999);
        let before_resend = rows_t(&core, &alice);
        assert_eq!(code(run(&core, &alice, &with_op)), "23505");
        assert_eq!(before_resend, rows_t(&core, &alice));
        let other = if kind == "UPD" { DEL } else { UPD };
        assert_eq!(code(run(&core, &alice, &op(other, "acc-1"))), "22023");
        assert_eq!(before_resend, rows_t(&core, &alice));
    }
}

/// 束縛エラー・変換エラー・上限超過は台帳を消費しない: 同じ op id で修正後の文を実行できる。
#[test]
fn failed_statements_do_not_consume_the_operation_id() {
    let alice = ctx("alice");
    let (core, _g) = build("sql53-acc-ledger-limit", |c| {
        c.with_dml_limits(dml_limit(1))
    });
    seed(&core, &alice);
    assert_eq!(code(run(&core, &alice, &op(UPD, "acc-lim"))), "54000");
    let fixed = op(
        "UPDATE t SET x = s.v FROM s WHERE t.k = s.k AND t.k = 'a'",
        "acc-lim",
    );
    assert_eq!(run(&core, &alice, &fixed).expect("fixed"), tag("UPD", 1));

    let (core, _g) = new_core("sql53-acc-ledger-bind");
    seed(&core, &alice);
    assert_eq!(
        code(run(
            &core,
            &alice,
            &op("UPDATE t SET x = s.nope FROM s WHERE t.k = s.k", "acc-b")
        )),
        "42703"
    );
    assert_eq!(
        code(run(
            &core,
            &alice,
            &op(
                "UPDATE t SET x = 1 FROM s WHERE t.k = s.k AND t.x < s.embedding",
                "acc-b"
            )
        )),
        "0A000"
    );
    assert_eq!(
        run(&core, &alice, &op(UPD, "acc-b")).expect("ok"),
        tag("UPD", 2)
    );
}

/// 明示 Tx 内で同じ op id を 2 度使うと拒否され、Tx は `Failed`（以降の文は `25P02`）。ROLLBACK で終われる。
#[test]
fn operation_id_reuse_inside_explicit_transaction_fails_the_transaction() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-acc-ledger-txn");
    seed(&core, &alice);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, &op(UPD, "acc-t"))
        .expect("first");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, &op(DEL, "acc-t"))
        .expect_err("reuse");
    assert!(
        ["22023", "23505", "25000"].contains(&err.wire_code()),
        "{}",
        err.wire_code()
    );
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "SELECT id FROM t LIMIT 1")
        .expect_err("failed tx");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("10") && !rows.contains("100"), "{rows}");
}

/// `--require-operation-id`（`LedgerMode::Ledgered`）では省略が `23502`、指定すれば通る。
#[test]
fn ledgered_mode_requires_operation_id_for_joined_dml() {
    let alice = ctx("alice");
    let (core, _g) = build("sql53-acc-ledgered", |c| {
        c.with_ledger_mode(LedgerMode::Ledgered)
    });
    // Ledgered 構成では INSERT も op id 必須のため、投入にも付ける。
    for (i, (table, id, k, n)) in [
        ("t", 1, "a", 10),
        ("t", 2, "b", 20),
        ("t", 3, "c", 30),
        ("s", 1, "a", 100),
        ("s", 2, "b", 200),
    ]
    .into_iter()
    .enumerate()
    {
        let col = if table == "t" { "x" } else { "v" };
        run(
            &core,
            &alice,
            &format!(
                "INSERT INTO {table} (id, embedding, k, {col}) VALUES ({id}, '[0.1,0.2]', '{k}', {n}) \
                 USING OPERATION_ID 'seed-{i}'"
            ),
        )
        .expect("seed");
    }
    let before = rows_t(&core, &alice);
    for sql in [UPD, DEL] {
        assert_eq!(code(run(&core, &alice, sql)), "23502", "{sql}");
        assert_eq!(before, rows_t(&core, &alice), "{sql}");
    }
    assert_eq!(
        run(&core, &alice, &op(UPD, "acc-l")).expect("ok"),
        tag("UPD", 2)
    );
}

// ---- 5. 明示トランザクション ----

/// COMMIT で永続化し、ROLLBACK で巻き戻る。`mark_written` の取りこぼしがあると COMMIT 後に消えるため、
/// 別の暗黙 Tx からの読み取りで永続化を確かめる。autocommit と結果が一致する。
#[test]
fn commit_persists_and_rollback_reverts() {
    let alice = ctx("alice");
    for (sql, kind) in [(UPD, "UPD"), (DEL, "DEL")] {
        let (core, _g) = new_core("sql53-acc-txn-finish");
        seed(&core, &alice);
        let before = rows_t(&core, &alice);
        assert_eq!(
            explicit_one(&core, &alice, sql, "ROLLBACK").expect(sql),
            tag(kind, 2)
        );
        assert_eq!(before, rows_t(&core, &alice), "ROLLBACK must revert {sql}");
        assert_eq!(
            explicit_one(&core, &alice, sql, "COMMIT").expect(sql),
            tag(kind, 2)
        );
        assert_ne!(before, rows_t(&core, &alice), "COMMIT must persist {sql}");
        let (auto, _g2) = new_core("sql53-acc-txn-auto");
        seed(&auto, &alice);
        run(&auto, &alice, sql).expect("auto");
        assert_eq!(rows_t(&auto, &alice), rows_t(&core, &alice), "{sql}");
    }
}

/// read-your-writes: 同一 Tx 内で INSERT した未コミット行を結合形が見て、結合形の結果を後続の SELECT が見る。
#[test]
fn joined_dml_reads_own_uncommitted_writes_and_is_visible_to_later_statements() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-acc-txn-ryw");
    seed(&core, &alice);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut step = |sql: &str| {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .map(|o| format!("{o:?}"))
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    step("BEGIN");
    step("INSERT INTO s (id, embedding, k, v) VALUES (7, '[0.1,0.2]', 'c', 333)");
    assert_eq!(step(UPD), tag("UPD", 3));
    let seen = step("SELECT id, x FROM t ORDER BY id LIMIT 100");
    assert!(seen.contains("333") && seen.contains("100"), "{seen}");
    assert_eq!(step(DEL), tag("DEL", 3));
    let seen = step("SELECT id, x FROM t ORDER BY id LIMIT 100");
    assert!(!seen.contains("333") && !seen.contains("100"), "{seen}");
    step("ROLLBACK");
    let rows = rows_t(&core, &alice);
    assert!(rows.contains("30") && rows.contains("10"), "{rows}");
}

/// 失敗した文（上限超過）で Tx は `Failed` になり、以降の文は `25P02`。ROLLBACK で行は元のまま。
#[test]
fn failed_joined_statement_fails_the_transaction() {
    let alice = ctx("alice");
    let (core, _g) = build("sql53-acc-txn-failed", |c| c.with_dml_limits(dml_limit(1)));
    seed(&core, &alice);
    let before = rows_t(&core, &alice);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, UPD)
        .expect_err("over limit");
    assert_eq!(err.wire_code(), "54000");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, DEL)
        .expect_err("failed tx");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(before, rows_t(&core, &alice));
}

// ---- 6. 自己結合 ----

/// 自己結合（別名で公開名を分ける）: 同キー 3 行の自己結合で、多重一致でも各行 1 回だけ更新し、値は走査順で
/// 最初の source 行（id=1。x=10）のもの。FROM 側列を含む算術式は未対応（`0A000`）で行は不変。
#[test]
fn self_join_update_applies_first_source_once_and_rejects_unsupported_expressions() {
    let alice = ctx("alice");
    for explicit in [false, true] {
        let (core, _g) = new_core("sql53-acc-self-upd");
        insert(&core, &alice, "t", 1, "a", 10);
        insert(&core, &alice, "t", 2, "a", 20);
        insert(&core, &alice, "t", 3, "a", 30);
        let before = rows_t(&core, &alice);
        let unsupported = "UPDATE t SET x = d2.x + t.x FROM t AS d2 WHERE t.k = d2.k";
        let r = if explicit {
            explicit_one(&core, &alice, unsupported, "ROLLBACK")
        } else {
            run(&core, &alice, unsupported).map_err(|e| e.wire_code().to_string())
        };
        assert_eq!(r.expect_err("expression"), "0A000");
        assert_eq!(before, rows_t(&core, &alice));
        let sql = "UPDATE t SET x = d2.x FROM t AS d2 WHERE t.k = d2.k";
        assert_eq!(exec(&core, &alice, sql, explicit), tag("UPD", 3));
        let rows = rows_t(&core, &alice);
        assert!(!rows.contains("20") && !rows.contains("30"), "{rows}");
    }
}

/// 自己結合の DELETE: 多重一致でも各行 1 回だけ消す。他テナントの同キー行は source にも対象にもならない。
#[test]
fn self_join_delete_counts_each_row_once_and_respects_tenants() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    for explicit in [false, true] {
        let (core, _g) = new_core("sql53-acc-self-del");
        insert(&core, &alice, "t", 1, "a", 10);
        insert(&core, &alice, "t", 2, "a", 20);
        insert(&core, &alice, "t", 3, "z", 30);
        insert(&core, &bob, "t", 9, "z", 90);
        let sql = "DELETE FROM t USING t AS d2 WHERE t.k = d2.k";
        assert_eq!(exec(&core, &alice, sql, explicit), tag("DEL", 3));
        let rows = rows_t(&core, &alice);
        assert!(!rows.contains("10") && !rows.contains("30"), "{rows}");
        assert!(rows_t(&core, &bob).contains("90"), "bob's row must remain");
    }
}

/// 自己結合 + RETURNING: 他テナントの行は返らず、source 側の別名の列を返せる。
#[test]
fn self_join_returning_exposes_only_own_rows() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-acc-self-ret");
    insert(&core, &alice, "t", 1, "a", 10);
    insert(&core, &bob, "t", 9, "a", 90);
    let out = run(
        &core,
        &alice,
        "UPDATE t SET x = d2.x FROM t AS d2 WHERE t.k = d2.k RETURNING t.id, d2.x",
    )
    .expect("update returning");
    assert!(out.contains("10") && !out.contains("90"), "{out}");
    assert!(rows_t(&core, &bob).contains("90"));
}

/// 括弧付きの対象表修飾列 `(t.k)`（別名付きを含む）は、FROM 側にも `k` があっても対象表へ解決される
/// （修飾を失って全スコープで再束縛され `42702` になってはならない）。
#[test]
fn parenthesized_target_qualified_returning_column_resolves_to_target() {
    let alice = ctx("alice");
    let (core, _g) = new_core("sql53-acc-paren-ret");
    insert(&core, &alice, "t", 1, "a", 10);
    for sql in [
        "UPDATE t SET x = d2.x FROM t AS d2 WHERE t.k = d2.k RETURNING (t.k)",
        "UPDATE t SET x = d2.x FROM t AS d2 WHERE t.k = d2.k RETURNING (t.k) AS kk",
    ] {
        let out = run(&core, &alice, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert!(out.contains('a'), "{sql}: {out}");
    }
}

// ---- 7. 末尾 `;` ----

/// psql 形の末尾 `;` 付き単独送信は、`;` なしと同じタグ・同じ副作用になる。
#[test]
fn trailing_semicolon_behaves_like_the_bare_statement() {
    let alice = ctx("alice");
    for (sql, kind) in [(UPD, "UPD"), (DEL, "DEL")] {
        let (a, _g1) = new_core("sql53-acc-semi-a");
        seed(&a, &alice);
        let (b, _g2) = new_core("sql53-acc-semi-b");
        seed(&b, &alice);
        assert_eq!(run(&a, &alice, sql).expect("bare"), tag(kind, 2));
        assert_eq!(
            run(&b, &alice, &format!("{sql};")).expect("semi"),
            tag(kind, 2)
        );
        assert_eq!(rows_t(&a, &alice), rows_t(&b, &alice));
    }
}

// ---- 8. 未対応の形（fail-closed） ----

/// 未対応の形は `0A000`／`42601`／束縛系で拒否され、行も台帳も不変。拒否のあとも同じ op id が使え、
/// 他テナントの行へ影響しない（RLS を緩める経路にならない）。
#[test]
fn unsupported_forms_fail_closed_without_side_effects() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-acc-reject");
    seed(&core, &alice);
    insert(&core, &bob, "t", 20, "a", 5);
    let before_a = rows_t(&core, &alice);
    let before_b = rows_t(&core, &bob);
    for (sql, want) in [
        ("DELETE FROM t USING s", "0A000"),
        ("UPDATE t SET x = 1 FROM s", "0A000"),
        // 非等値・OR をまたぐ述語は Issue #3295 で受理する（`sql53_dml_from_using_residual.rs`）。
        // 残余が VECTOR 列を参照する形は引き続き fail-closed。
        (
            "UPDATE t SET x = 1 FROM s WHERE t.k = s.k AND t.x < s.embedding",
            "0A000",
        ),
        (
            "DELETE FROM t USING s WHERE t.k = s.k OR s.embedding = t.x",
            "0A000",
        ),
        (
            "DELETE FROM t USING s, s AS s2 WHERE t.k = s.k AND s.k = s2.k",
            "0A000",
        ),
        ("DELETE FROM t USING nope WHERE t.k = nope.k", "42P01"),
        ("UPDATE t SET x = s.nope FROM s WHERE t.k = s.k", "42703"),
        ("UPDATE t SET x = s.k FROM s WHERE t.k = s.k", "42804"),
        ("DELETE FROM t USING s WHERE t.k = s.k PARTITIONED", "42601"),
        ("DELETE FROM t USING s WHERE t.k = s.k extra", "42601"),
    ] {
        let got = code(run(&core, &alice, sql));
        assert_eq!(got, want, "{sql}");
        if want != "42601" {
            // 拒否された文は op id を消費しない（同じ op id を付けても再送扱い 23505 にならない）。
            let with_op = format!("{sql} USING OPERATION_ID 'acc-rej'");
            assert_ne!(code(run(&core, &alice, &with_op)), "23505", "{sql}");
        }
    }
    assert_eq!(before_a, rows_t(&core, &alice));
    assert_eq!(before_b, rows_t(&core, &bob));
    assert_eq!(
        run(&core, &alice, &op(UPD, "acc-rej")).expect("ok"),
        tag("UPD", 2)
    );
}
