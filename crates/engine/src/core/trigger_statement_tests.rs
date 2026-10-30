//! 文レベルトリガ（FOR EACH STATEMENT）と TRUNCATE トリガの発火の結合テスト（TABLE-45・SQL-32・Issue #1726）。
//!
//! トリガ関数の実体（Issue #2228）が未整備のため、定義をカタログへ直接注入し、呼び出し口を差し替えた
//! invoker が `TG_WHEN`・`TG_LEVEL`・`TG_OP` を記録する。1 文 1 回の発火・対象 0 行での発火・行トリガとの
//! 順序・TRUNCATE の全表 BEFORE → 削除 → 全表 AFTER の順序・再送でトリガを呼ばないこと・未結線経路の
//! `0A000`・テナント境界を `EngineCore` に SQL を流す水準で固定する。
//! 設計記録は `docs/design/trigger-statement-truncate.md`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{
    add_trigger, core_with, count, ctx, read_rows, run, trigger_def, Behavior,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

const INS: u8 = TriggerEvents::INSERT;
const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;
const TRUNC: u8 = TriggerEvents::TRUNCATE;

type Log = Arc<Mutex<Vec<String>>>;

fn trigger(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut def = trigger_def(name, timing, level);
    def.events = TriggerEvents::from_bits(events).expect("events");
    def
}

fn stmt(name: &str, timing: TriggerTiming, events: u8) -> TriggerDef {
    trigger(name, timing, TriggerLevel::Statement, events)
}

fn row(name: &str, timing: TriggerTiming, events: u8) -> TriggerDef {
    trigger(name, timing, TriggerLevel::Row, events)
}

/// 呼び出しを `TG_WHEN TG_LEVEL TG_OP table` の形で記録し、行は変えない。
fn logging(log: &Log) -> Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        log.lock().expect("log").push(format!(
            "{} {} {} {}",
            c.tg_when(),
            c.tg_level(),
            c.tg_op(),
            c.table
        ));
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn code(core: &EngineCore, c: &crate::policy::PolicyContext, sql: &str) -> &'static str {
    run(core, c, sql).expect_err(sql).wire_code()
}

/// `trg` へ 3 行 `(1,'x',10)`・`(2,'y',20)`・`(3,'z',30)` を入れたコア（トリガは入れる前に無い）。
fn seeded(dir: &std::path::Path, behavior: Behavior) -> (EngineCore, Arc<AtomicUsize>) {
    let (core, calls) = core_with(dir, behavior);
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
         (2, '[2.0,0.0]', 'y', 20), (3, '[3.0,0.0]', 'z', 30)",
    )
    .expect("seed");
    (core, calls)
}

fn both_statement_triggers(core: &EngineCore, table: &str, events: u8) {
    add_trigger(
        &core.storage,
        "tenant-a",
        table,
        &stmt("a_before", TriggerTiming::Before, events),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        table,
        &stmt("b_after", TriggerTiming::After, events),
    );
}

// 対象が 0 行でも、BEFORE／AFTER の文トリガは 1 回ずつ発火する（受け入れ条件）。述語形・id 指定形の
// UPDATE／DELETE、空の表への INSERT ... SELECT・TRUNCATE。行トリガは呼ばれない。
#[test]
fn statement_triggers_fire_once_even_when_no_rows_match() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    both_statement_triggers(&core, "trg", INS | UPD | DEL | TRUNC);
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &row("r_row", TriggerTiming::Before, UPD | DEL),
    );
    take(&log);

    for (sql, op) in [
        ("UPDATE trg SET n = 1 WHERE n > 1000", "UPDATE"),
        ("DELETE FROM trg WHERE n > 1000", "DELETE"),
        ("UPDATE trg SET n = 1 WHERE id = 99", "UPDATE"),
        ("DELETE FROM trg WHERE id = 99", "DELETE"),
    ] {
        run(&core, &a, sql).expect(sql);
        assert_eq!(
            take(&log),
            vec![
                format!("BEFORE STATEMENT {op} trg"),
                format!("AFTER STATEMENT {op} trg")
            ],
            "{sql}"
        );
    }

    // 空の表（plain）への INSERT ... SELECT（結果 0 行）。INSERT 文トリガは plain 側に付ける。
    both_statement_triggers(&core, "plain", INS);
    let out = run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a, n) SELECT id, embedding, a, n FROM plain LIMIT 100 \
         USING OPERATION_ID 'op-empty'",
    )
    .expect("insert select 0 rows");
    assert!(matches!(out, SqlOutcome::Insert(_)));
    assert_eq!(
        take(&log),
        vec![
            "BEFORE STATEMENT INSERT plain".to_string(),
            "AFTER STATEMENT INSERT plain".to_string()
        ]
    );
    // 同じ operation_id の再送は台帳で拒否され、トリガは 1 つも呼ばれない。
    assert_eq!(
        code(
            &core,
            &a,
            "INSERT INTO plain (id, embedding, a, n) SELECT id, embedding, a, n FROM plain \
             LIMIT 100 USING OPERATION_ID 'op-empty'"
        ),
        "23505"
    );
    assert!(take(&log).is_empty());
}

// 不在の id・他テナントの行の id では、呼び出し列も応答も同じ（存在情報を漏らさない）。
#[test]
fn id_form_zero_rows_does_not_reveal_other_tenants() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    run(
        &core,
        &ctx("tenant-b"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (50, '[1.0,0.0]', 'b', 1)",
    )
    .expect("tenant-b row");
    let a = ctx("tenant-a");
    both_statement_triggers(&core, "trg", UPD | DEL);
    take(&log);

    for template in [
        "UPDATE trg SET n = 1 WHERE id = {}",
        "DELETE FROM trg WHERE id = {}",
    ] {
        let mut seen = Vec::new();
        for id in [99, 50] {
            let sql = template.replace("{}", &id.to_string());
            let out = run(&core, &a, &sql).expect(&sql);
            let affected = match out {
                SqlOutcome::Update(o) => o.rows_affected,
                SqlOutcome::Delete(o) => o.rows_affected,
                _ => panic!("unexpected outcome"),
            };
            seen.push((affected, take(&log)));
        }
        assert_eq!(seen[0], seen[1], "{template}");
        assert_eq!(seen[0].0, 0);
        assert_eq!(seen[0].1.len(), 2);
    }
    // tenant-b の行は無傷。
    assert_eq!(count(&core, &ctx("tenant-b"), "trg"), 1);
}

// 複数行の文でも文トリガは 1 回。行トリガとの順序は BEFORE STMT → BEFORE ROW → AFTER ROW → AFTER STMT。
#[test]
fn statement_triggers_fire_once_per_statement_around_row_triggers() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    both_statement_triggers(&core, "trg", INS | UPD | DEL);
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &row("c_brow", TriggerTiming::Before, INS | UPD | DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &row("d_arow", TriggerTiming::After, INS | UPD | DEL),
    );
    take(&log);

    let expected = |op: &str, n: usize| {
        let mut v = vec![format!("BEFORE STATEMENT {op} trg")];
        v.extend((0..n).map(|_| format!("BEFORE ROW {op} trg")));
        v.extend((0..n).map(|_| format!("AFTER ROW {op} trg")));
        v.push(format!("AFTER STATEMENT {op} trg"));
        v
    };
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (11, '[1.0,0.0]', 'p', 1), \
         (12, '[1.0,0.0]', 'q', 2), (13, '[1.0,0.0]', 'r', 3)",
    )
    .expect("insert");
    assert_eq!(take(&log), expected("INSERT", 3));
    run(&core, &a, "UPDATE trg SET a = 'u' WHERE n >= 20").expect("update");
    assert_eq!(take(&log), expected("UPDATE", 2));
    run(&core, &a, "DELETE FROM trg WHERE n >= 20").expect("delete");
    assert_eq!(take(&log), expected("DELETE", 2));
}

// 同じ時点の文トリガが複数あれば名前のバイト順。BEFORE ROW が全行を NULL で飛ばしても AFTER STATEMENT は発火し、
// 影響行数は 0。
#[test]
fn statement_trigger_order_and_all_rows_skipped() {
    let dir = tempdir();
    let log = new_log();
    let log2 = Arc::clone(&log);
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(move |c| {
            log2.lock().expect("log").push(format!(
                "{} {} {}",
                c.trigger_name,
                c.tg_when(),
                c.tg_level()
            ));
            if c.tg_level() == "ROW" {
                Ok(TriggerOutcome::Skip)
            } else {
                Ok(TriggerOutcome::Proceed(Vec::new()))
            }
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("z_after", TriggerTiming::After, DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("b_after", TriggerTiming::After, DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("m_before", TriggerTiming::Before, DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &row("r", TriggerTiming::Before, DEL),
    );
    take(&log);
    let out = run(&core, &a, "DELETE FROM trg WHERE n > 0").expect("delete");
    let SqlOutcome::Delete(o) = out else {
        panic!("delete")
    };
    assert_eq!(o.rows_affected, 0);
    assert_eq!(count(&core, &a, "trg"), 3);
    let entries = take(&log);
    let stmt_order: Vec<&str> = entries
        .iter()
        .filter(|e| e.ends_with("STATEMENT"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        stmt_order,
        vec![
            "m_before BEFORE STATEMENT",
            "b_after AFTER STATEMENT",
            "z_after AFTER STATEMENT"
        ]
    );
    assert_eq!(entries.iter().filter(|e| e.ends_with("ROW")).count(), 3);
}

// BEFORE STATEMENT が書いた行は外側の DELETE の対象に入り、AFTER STATEMENT が書いた行は入らない（D5）。
#[test]
fn before_statement_writes_are_visible_to_target_and_after_are_not() {
    let dir = tempdir();
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(|c| {
            let id = if c.tg_when() == "BEFORE" { 100 } else { 200 };
            c.writes.insert_row(
                "trg",
                Some(id),
                &[
                    Value::Vector(vec![1.0, 0.0]),
                    Value::Text("t".to_string()),
                    Value::BigInt(5),
                ],
            )?;
            Ok(TriggerOutcome::Proceed(Vec::new()))
        }),
    );
    let a = ctx("tenant-a");
    both_statement_triggers(&core, "trg", DEL);
    let out = run(&core, &a, "DELETE FROM trg WHERE n >= 0").expect("delete");
    let SqlOutcome::Delete(o) = out else {
        panic!("delete")
    };
    assert_eq!(o.rows_affected, 4);
    let rows = read_rows(&core, &a, "trg");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 200);
}

// `UPDATE OF` の文トリガは SET 列と交差するときだけ発火する（0 行でも SET 列で判定する）。
#[test]
fn update_of_statement_trigger_follows_set_columns() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    let mut def = stmt("s", TriggerTiming::After, UPD);
    def.update_of = vec!["n".to_string()];
    add_trigger(&core.storage, "tenant-a", "trg", &def);
    take(&log);
    run(&core, &a, "UPDATE trg SET a = 'q' WHERE n > 1000").expect("a");
    assert!(take(&log).is_empty());
    run(&core, &a, "UPDATE trg SET n = 1 WHERE n > 1000").expect("n");
    assert_eq!(take(&log), vec!["AFTER STATEMENT UPDATE trg".to_string()]);
}

// 文トリガの失敗は文全体を取り消す（BEFORE／AFTER とも）。行・台帳は残らない。
#[test]
fn statement_trigger_error_aborts_the_statement() {
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
        let dir = tempdir();
        let (core, _calls) = seeded(
            dir.path(),
            Box::new(|c| {
                if c.tg_level() == "STATEMENT" {
                    Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "boom".to_string(),
                    })
                } else {
                    Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
                }
            }),
        );
        let a = ctx("tenant-a");
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &stmt("s", timing, DEL | INS),
        );
        assert_eq!(
            code(
                &core,
                &a,
                "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'op-del'"
            ),
            "0A000"
        );
        assert_eq!(count(&core, &a, "trg"), 3);
        assert_eq!(
            code(
                &core,
                &a,
                "INSERT INTO trg (id, embedding, a, n) VALUES (9, '[1.0,0.0]', 'x', 1)"
            ),
            "0A000"
        );
        assert_eq!(count(&core, &a, "trg"), 3);
    }
}

// 文トリガの `WHEN` は発火時にも `0A000`。
#[test]
fn statement_trigger_with_when_is_0a000() {
    let dir = tempdir();
    let (core, calls) = core_with(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    let a = ctx("tenant-a");
    let mut def = stmt("w", TriggerTiming::Before, INS);
    def.when_sql = Some("true".to_string());
    add_trigger(&core.storage, "tenant-a", "trg", &def);
    assert_eq!(
        code(
            &core,
            &a,
            "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)"
        ),
        "0A000"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 発火に未対応の経路（RETURNING・UPSERT・分割実行）は文トリガでも `0A000`。呼び出しは 0 回。
#[test]
fn unwired_paths_stay_guarded_for_statement_triggers() {
    let dir = tempdir();
    let (core, calls) = seeded(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("s", TriggerTiming::After, INS | UPD | DEL),
    );
    let before = calls.load(Ordering::SeqCst);
    for sql in [
        "UPDATE trg SET n = 1 WHERE id = 1 RETURNING id USING OPERATION_ID 'g1'",
        "DELETE FROM trg WHERE n > 0 RETURNING id USING OPERATION_ID 'g2'",
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO UPDATE SET n = 2 USING OPERATION_ID 'g3'",
        "UPDATE trg SET n = 1 WHERE n > 0 USING OPERATION_ID 'g4' PARTITIONED CHUNK 1",
        "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'g5' PARTITIONED CHUNK 1",
    ] {
        assert_eq!(code(&core, &a, sql), "0A000", "{sql}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(count(&core, &a, "trg"), 3);
}

// BEFORE STATEMENT UPDATE しか無い表では、NOT NULL の検査を延期しない（`23502` と `UPDATE 0` の差で
// 行の存在を漏らさない）。該当行の有無にかかわらず束縛時に `23502`。省略した NOT NULL 列の INSERT も事前に `23502`。
#[test]
fn statement_triggers_do_not_defer_not_null() {
    let dir = tempdir();
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("s", TriggerTiming::Before, INS | UPD),
    );
    assert_eq!(
        code(&core, &a, "UPDATE trg SET n = NULL WHERE id = 1"),
        "23502"
    );
    assert_eq!(
        code(&core, &a, "UPDATE trg SET n = NULL WHERE id = 99"),
        "23502"
    );
    assert_eq!(
        code(
            &core,
            &a,
            "INSERT INTO trg (id, embedding, a) VALUES (9, '[1.0,0.0]', 'x')"
        ),
        "23502"
    );
}

// 文トリガは定義したテナントの書き込みでだけ発火する。
#[test]
fn statement_triggers_fire_only_for_the_defining_tenant() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    both_statement_triggers(&core, "trg", INS | DEL);
    take(&log);
    run(
        &core,
        &ctx("tenant-b"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (70, '[1.0,0.0]', 'b', 1)",
    )
    .expect("tenant-b insert");
    run(&core, &ctx("tenant-b"), "DELETE FROM trg WHERE n > 0").expect("tenant-b delete");
    assert!(take(&log).is_empty());
}

// TRUNCATE: `TG_OP`・`TG_LEVEL`・全表の BEFORE → 削除 → 全表の AFTER。DELETE トリガは発火しない。
#[test]
fn truncate_fires_statement_triggers_per_table_in_order() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'p', 1)",
    )
    .expect("plain");
    for table in ["trg", "plain"] {
        both_statement_triggers(&core, table, TRUNC);
        add_trigger(
            &core.storage,
            "tenant-a",
            table,
            &stmt("del_s", TriggerTiming::After, DEL),
        );
    }
    take(&log);

    run(&core, &a, "TRUNCATE trg, plain USING OPERATION_ID 'op-t1'").expect("truncate");
    assert_eq!(
        take(&log),
        vec![
            "BEFORE STATEMENT TRUNCATE trg".to_string(),
            "BEFORE STATEMENT TRUNCATE plain".to_string(),
            "AFTER STATEMENT TRUNCATE trg".to_string(),
            "AFTER STATEMENT TRUNCATE plain".to_string(),
        ]
    );
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "plain"), 0);

    // 空の表への TRUNCATE でも発火する。同じ operation_id の再送はトリガを呼ばずに `23505`。
    run(&core, &a, "TRUNCATE trg USING OPERATION_ID 'op-t2'").expect("empty truncate");
    assert_eq!(take(&log).len(), 2);
    assert_eq!(
        code(&core, &a, "TRUNCATE trg USING OPERATION_ID 'op-t2'"),
        "23505"
    );
    assert!(take(&log).is_empty());
}

// TRUNCATE トリガの失敗は文全体を取り消す（行が残り、台帳も無い）。BEFORE・AFTER の両方。
#[test]
fn truncate_trigger_error_rolls_everything_back() {
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
        let dir = tempdir();
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing2 = Arc::clone(&failing);
        let (core, _calls) = seeded(
            dir.path(),
            Box::new(move |_| {
                if failing2.load(Ordering::SeqCst) {
                    Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "boom".to_string(),
                    })
                } else {
                    Ok(TriggerOutcome::Proceed(Vec::new()))
                }
            }),
        );
        let a = ctx("tenant-a");
        add_trigger(&core.storage, "tenant-a", "trg", &stmt("s", timing, TRUNC));
        assert_eq!(
            code(&core, &a, "TRUNCATE trg USING OPERATION_ID 'op-x'"),
            "0A000"
        );
        assert_eq!(count(&core, &a, "trg"), 3);
        // 台帳が残っていれば、同じ operation_id の再実行が `23505` になる（残っていないので成功する）。
        failing.store(false, Ordering::SeqCst);
        run(&core, &a, "TRUNCATE trg USING OPERATION_ID 'op-x'").expect("retry");
        assert_eq!(count(&core, &a, "trg"), 0);
    }
}

// RESTART IDENTITY と TRUNCATE トリガの組み合わせは `0A000`（副作用ゼロ）。
#[test]
fn truncate_restart_identity_with_triggers_is_0a000() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("s", TriggerTiming::After, TRUNC),
    );
    take(&log);
    assert_eq!(
        code(
            &core,
            &a,
            "TRUNCATE trg RESTART IDENTITY USING OPERATION_ID 'op-r'"
        ),
        "0A000"
    );
    assert!(take(&log).is_empty());
    assert_eq!(count(&core, &a, "trg"), 3);
}

// 呼び出し口（通過証）の無い公開入口では、TRUNCATE トリガがあれば `0A000` で行が残る。
#[test]
fn truncate_without_runtime_is_0a000() {
    let dir = tempdir();
    let (core, calls) = seeded(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &stmt("s", TriggerTiming::Before, TRUNC),
    );
    let validated = crate::sql::allowlist::validate_truncate(
        "TRUNCATE trg USING OPERATION_ID 'op-pub'",
        &core.storage,
        core.ledger_mode,
    )
    .expect("validate");
    let err = crate::sql::exec::execute_truncate(&core.storage, &a, &validated, core.ledger_mode)
        .expect_err("unwired");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 3);
}

// 明示トランザクション内の TRUNCATE でも発火する。ROLLBACK すれば行は戻る。
#[test]
fn truncate_fires_inside_explicit_transaction() {
    let dir = tempdir();
    let log = new_log();
    let (core, _calls) = seeded(dir.path(), logging(&log));
    let a = ctx("tenant-a");
    both_statement_triggers(&core, "trg", TRUNC);
    take(&log);
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "TRUNCATE trg USING OPERATION_ID 'op-tx'",
        "ROLLBACK",
    ] {
        core.execute_sql_in_txn(&a, &mut s, &mut txn, sql)
            .expect(sql);
    }
    assert_eq!(take(&log).len(), 2);
    assert_eq!(count(&core, &a, "trg"), 3);
}
