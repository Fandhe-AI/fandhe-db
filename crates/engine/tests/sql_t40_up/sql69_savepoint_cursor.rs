//! `ROLLBACK TO` によるカーソルの整合（Issue #1946・`docs/spec/04-behavior/sql-surface.md`
//! SQL-69 (5) と WIRE-15 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、savepoint より後に宣言した
//! カーソルは `ROLLBACK TO` で閉じ（以降の `FETCH` は `34000`）、前に宣言したカーソルは
//! 開いたまま位置・中身を保つことを固定する。閉鎖の実装は `CursorRegistry::close_declared_since`
//! （水位による閉鎖）で、本ファイルはその外から見た契約を検査する。時間計測には依存しない。

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::cursor::MAX_CURSORS_PER_SESSION;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const SEED_ROWS: u64 = 5;

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // Tx の外で確定済みの行を用意する（カーソルの中身の期待値はこの集合）。
    let mut s = SessionState::default();
    let caller = ctx();
    for id in 1..=SEED_ROWS {
        core.execute_sql_in_session(&caller, &mut s, &insert_sql(id, &format!("seed-{id}")))
            .expect("seed insert");
    }
    (core, guard)
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

fn declare_sql(name: &str) -> String {
    format!("DECLARE {name} CURSOR FOR SELECT id FROM {TABLE} LIMIT 100")
}

/// 1 セッション分の Tx 文脈。
struct Conn<'e> {
    core: &'e EngineCore,
    caller: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Conn<'e> {
    fn begin(core: &'e EngineCore) -> Self {
        let mut c = Self {
            core,
            caller: ctx(),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        };
        c.ok("BEGIN");
        c
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core
            .execute_sql_in_txn(&self.caller, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) {
        self.run(sql)
            .unwrap_or_else(|e| panic!("`{sql}` failed: {e:?}"));
    }

    fn fetch_ids(&mut self, sql: &str) -> Vec<u64> {
        match self.run(sql).expect("fetch") {
            SqlOutcome::Fetch(result) => result.rows.iter().map(|r| r.id).collect(),
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    /// 閉じたカーソルへの `FETCH` が `34000` で拒否され、Tx が `Failed` になることを確かめる。
    fn assert_fetch_rejected_and_failed(&mut self, name: &str) {
        let err = self
            .run(&format!("FETCH 1 FROM {name}"))
            .expect_err("closed cursor must be rejected");
        assert_eq!(err.wire_code(), "34000");
        assert_eq!(self.txn.status(), TransactionStatus::Failed);
    }
}

fn seeded_ids() -> BTreeSet<u64> {
    (1..=SEED_ROWS).collect()
}

#[test]
fn cursor_declared_after_savepoint_is_closed() {
    let (core, _g) = new_core("sql69-cur-after");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    c.ok(&insert_sql(100, "op-100"));
    c.ok(&declare_sql("c"));
    c.ok("ROLLBACK TO a");
    assert_eq!(c.txn.status(), TransactionStatus::InTransaction);
    c.assert_fetch_rejected_and_failed("c");
    // Failed 後の文は 25P02 で拒否される。
    let err = c.run("FETCH 1 FROM c").expect_err("failed txn");
    assert_eq!(err.wire_code(), "25P02");
}

#[test]
fn cursor_declared_before_savepoint_keeps_position() {
    let (core, _g) = new_core("sql69-cur-pos");
    let mut c = Conn::begin(&core);
    c.ok(&declare_sql("b"));
    let mut seen = c.fetch_ids("FETCH 1 FROM b");
    assert_eq!(seen.len(), 1);
    c.ok("SAVEPOINT a");
    seen.extend(c.fetch_ids("FETCH 2 FROM b"));
    assert_eq!(seen.len(), 3);
    c.ok("ROLLBACK TO a");
    // 位置は戻らない。続きの 2 件だけが返り、重複しない。
    let rest = c.fetch_ids("FETCH 10 FROM b");
    assert_eq!(rest.len(), 2);
    seen.extend(rest);
    let all: BTreeSet<u64> = seen.iter().copied().collect();
    assert_eq!(all.len(), seen.len(), "no row is returned twice");
    assert_eq!(all, seeded_ids());
}

#[test]
fn cursor_declared_before_savepoint_does_not_show_rolled_back_rows() {
    let (core, _g) = new_core("sql69-cur-content");
    let mut c = Conn::begin(&core);
    c.ok(&declare_sql("b"));
    c.ok("SAVEPOINT a");
    c.ok(&insert_sql(100, "op-100"));
    c.ok("ROLLBACK TO a");
    let ids: BTreeSet<u64> = c.fetch_ids("FETCH 100 FROM b").into_iter().collect();
    assert_eq!(ids, seeded_ids());
}

#[test]
fn close_is_not_undone_by_rollback_to() {
    let (core, _g) = new_core("sql69-cur-close");
    let mut c = Conn::begin(&core);
    c.ok(&declare_sql("b"));
    c.ok("SAVEPOINT a");
    c.ok("CLOSE b");
    c.ok("ROLLBACK TO a");
    c.assert_fetch_rejected_and_failed("b");
}

#[test]
fn same_name_can_be_redeclared_after_rollback_and_is_closed_again() {
    let (core, _g) = new_core("sql69-cur-redeclare");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    c.ok(&insert_sql(100, "op-100"));
    c.ok(&declare_sql("c"));
    c.ok("ROLLBACK TO a");
    // 閉じた名前は空き。再宣言でき、巻き戻した行を含まない。
    c.ok(&declare_sql("c"));
    let ids: BTreeSet<u64> = c.fetch_ids("FETCH 100 FROM c").into_iter().collect();
    assert_eq!(ids, seeded_ids());
    // savepoint は残っており、再宣言したカーソルも次の巻き戻しで閉じる。
    c.ok("ROLLBACK TO a");
    c.assert_fetch_rejected_and_failed("c");
}

#[test]
fn release_does_not_close_cursors() {
    let (core, _g) = new_core("sql69-cur-release");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    c.ok(&declare_sql("c"));
    c.ok("RELEASE SAVEPOINT a");
    let ids: BTreeSet<u64> = c.fetch_ids("FETCH 100 FROM c").into_iter().collect();
    assert_eq!(ids, seeded_ids());
}

#[test]
fn nested_savepoints_close_cursors_level_by_level() {
    let (core, _g) = new_core("sql69-cur-nested");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    c.ok(&declare_sql("x"));
    c.ok("SAVEPOINT b");
    c.ok(&declare_sql("y"));
    c.ok("ROLLBACK TO b");
    // x は残り、y は閉じる。
    assert_eq!(c.fetch_ids("FETCH 100 FROM x").len(), SEED_ROWS as usize);
    c.assert_fetch_rejected_and_failed("y");

    // 別の Tx で a への巻き戻しが x も閉じることを確かめる。
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    c.ok(&declare_sql("x"));
    c.ok("SAVEPOINT b");
    c.ok(&declare_sql("y"));
    c.ok("ROLLBACK TO a");
    c.assert_fetch_rejected_and_failed("x");
}

#[test]
fn rollback_to_frees_cursor_count_slots() {
    let (core, _g) = new_core("sql69-cur-limit");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT a");
    for i in 0..MAX_CURSORS_PER_SESSION {
        c.ok(&declare_sql(&format!("c{i}")));
    }
    c.ok("ROLLBACK TO a");
    for i in 0..MAX_CURSORS_PER_SESSION {
        c.ok(&declare_sql(&format!("d{i}")));
    }
}
// --- SCROLL・MOVE と ROLLBACK TO（Issue #3337・WIRE-27 (3) のポインタ。PG17 17.11 実測と一致） ---

fn declare_scroll_sql(name: &str) -> String {
    format!("DECLARE {name} SCROLL CURSOR FOR SELECT id FROM {TABLE} LIMIT 100")
}

impl Conn<'_> {
    fn move_n(&mut self, sql: &str) -> u64 {
        match self.run(sql).expect("move") {
            SqlOutcome::Move(n) => n,
            other => panic!("expected Move, got {other:?}"),
        }
    }
}

/// savepoint の前に宣言した SCROLL カーソルの位置は、savepoint 内の逆方向・絶対位置の移動を
/// `ROLLBACK TO` しても巻き戻らない（PG17 と同じ）。
#[test]
fn scroll_cursor_position_is_not_rolled_back_by_rollback_to() {
    let (core, _g) = new_core("sql69-scroll-pos");
    let mut c = Conn::begin(&core);
    c.ok(&declare_scroll_sql("c"));
    assert_eq!(c.fetch_ids("FETCH 2 FROM c"), vec![1, 2]);
    c.ok("SAVEPOINT s");
    assert_eq!(c.fetch_ids("FETCH LAST FROM c"), vec![5]);
    assert_eq!(c.move_n("MOVE BACKWARD 2 FROM c"), 2);
    c.ok("ROLLBACK TO s");
    assert_eq!(c.txn.status(), TransactionStatus::InTransaction);
    assert_eq!(c.fetch_ids("FETCH NEXT FROM c"), vec![4]);

    // MOVE ABSOLUTE 0 も同様に戻らない。
    c.ok("SAVEPOINT t");
    assert_eq!(c.move_n("MOVE ABSOLUTE 0 FROM c"), 0);
    c.ok("ROLLBACK TO t");
    assert_eq!(c.fetch_ids("FETCH NEXT FROM c"), vec![1]);
}

/// savepoint より後に宣言した SCROLL カーソルは `ROLLBACK TO` で閉じ、`MOVE`・`FETCH PRIOR`・
/// `FETCH LAST` はいずれも `34000`。同じ名前は再宣言でき、位置 0 から始まる。
#[test]
fn scroll_cursor_declared_after_savepoint_is_closed_for_every_statement() {
    for stmt in [
        "MOVE FORWARD 1 FROM c",
        "MOVE BACKWARD ALL FROM c",
        "FETCH PRIOR FROM c",
        "FETCH LAST FROM c",
    ] {
        let (core, _g) = new_core("sql69-scroll-closed");
        let mut c = Conn::begin(&core);
        c.ok("SAVEPOINT s");
        c.ok(&declare_scroll_sql("c"));
        c.ok("FETCH 2 FROM c");
        c.ok("ROLLBACK TO s");
        let err = c.run(stmt).expect_err(stmt);
        assert_eq!(err.wire_code(), "34000", "{stmt}");
        assert_eq!(c.txn.status(), TransactionStatus::Failed, "{stmt}");
    }
    let (core, _g) = new_core("sql69-scroll-redeclare");
    let mut c = Conn::begin(&core);
    c.ok("SAVEPOINT s");
    c.ok(&declare_scroll_sql("c"));
    c.ok("FETCH 2 FROM c");
    c.ok("ROLLBACK TO s");
    c.ok(&declare_scroll_sql("c"));
    assert_eq!(c.fetch_ids("FETCH NEXT FROM c"), vec![1]);
}

/// 別の文のエラー（存在しない表）の後の `ROLLBACK TO` でも、savepoint 前のカーソルは続きから動く。
#[test]
fn scroll_cursor_survives_unrelated_statement_error_and_rollback_to() {
    let (core, _g) = new_core("sql69-scroll-other-err");
    let mut c = Conn::begin(&core);
    c.ok(&declare_scroll_sql("c"));
    assert_eq!(c.fetch_ids("FETCH 3 FROM c"), vec![1, 2, 3]);
    c.ok("SAVEPOINT s");
    let err = c
        .run("SELECT id FROM missing_table LIMIT 1")
        .expect_err("missing table");
    assert_ne!(err.wire_code(), "34000");
    assert_eq!(c.txn.status(), TransactionStatus::Failed);
    c.ok("ROLLBACK TO s");
    assert_eq!(c.move_n("MOVE PRIOR FROM c"), 1);
    assert_eq!(c.fetch_ids("FETCH NEXT FROM c"), vec![3]);
}

/// NO SCROLL の `MOVE BACKWARD` が `55000` で拒否されても位置は変わらず、`ROLLBACK TO` の後は続きから動く
/// （`sql_cursor.rs` の `FETCH` 版と同じ契約。PG17 は同じ形でカーソルを使えなくするが、本実装は
/// 既存の契約に従う。差は `docs/design/sql-cursor.md`「PG17 との意図した差」）。
#[test]
fn no_scroll_move_rejection_keeps_position_after_rollback_to() {
    let (core, _g) = new_core("sql69-noscroll-move");
    let mut c = Conn::begin(&core);
    c.ok("DECLARE c NO SCROLL CURSOR FOR SELECT id FROM documents LIMIT 100");
    assert_eq!(c.move_n("MOVE 2 FROM c"), 2);
    c.ok("SAVEPOINT s");
    let err = c.run("MOVE BACKWARD 1 FROM c").expect_err("no scroll");
    assert_eq!(err.wire_code(), "55000");
    assert_eq!(c.txn.status(), TransactionStatus::Failed);
    c.ok("ROLLBACK TO s");
    assert_eq!(c.move_n("MOVE NEXT FROM c"), 1);
    assert_eq!(c.fetch_ids("FETCH NEXT FROM c"), vec![4]);
}
