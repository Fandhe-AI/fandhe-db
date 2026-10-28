//! 相関 `EXISTS` の残余評価に関する追加の結合テスト（SQL-61・Issue #1842 のレビュー対応）。
//!
//! 1. `CHAR(n)` 列同士の等価は結合キーにならず残余評価へ回るため、`0A000` で拒否せず
//!    既存の CHAR 比較意味論（末尾空白詰めの正規化）で評価する。
//! 2. キーなし相関の残余評価は外側 1 行で内側全行を走査するため、内側の比較ループでも
//!    協調的中断（`engine::interrupt`）を確認し `57014` で打ち切る。

use std::cell::Cell;
use std::rc::Rc;

use engine::core::EngineCore;
use engine::interrupt::{InterruptReason, InterruptScope};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str, ddls: &[&str]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in ddls {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
}

fn row_count(core: &EngineCore, sql: &str) -> usize {
    match run(core, sql).unwrap_or_else(|e| panic!("sql={sql:?}: {e:?}")) {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn char_equality_in_correlated_exists_is_evaluated_not_rejected() {
    let (core, _g) = open(
        "sql61-char",
        &[
            "CREATE TABLE a (id BIGINT PRIMARY KEY, c CHAR(4))",
            "CREATE TABLE b (id BIGINT PRIMARY KEY, c CHAR(4))",
        ],
    );
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO a (id, c) VALUES (1, 'ab'), (2, 'cd'), (3, 'ef')",
    )
    .expect("seed a");
    core.execute_insert_sql(&ctx(), "INSERT INTO b (id, c) VALUES (1, 'ab'), (2, 'ef')")
        .expect("seed b");
    // a1 ('ab')・a3 ('ef') が b と一致し、a2 ('cd') は一致しない。
    assert_eq!(
        row_count(
            &core,
            "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.c = a.c)"
        ),
        2
    );
}

/// 外側 1 行・内側多数行のキーなし相関で、残余評価の内側ループが中断を確認して
/// `57014` になる（確認点が無ければ完走してしまう）。
#[test]
fn keyless_residual_loop_polls_for_interrupt() {
    let (core, _g) = open(
        "sql61-interrupt",
        &[
            "CREATE TABLE a (id BIGINT PRIMARY KEY, d DOUBLE PRECISION)",
            "CREATE TABLE b (id BIGINT PRIMARY KEY, d DOUBLE PRECISION)",
        ],
    );
    core.execute_insert_sql(&ctx(), "INSERT INTO a (id, d) VALUES (1, -1.0)")
        .expect("seed a");
    for chunk in 0..10_i64 {
        let values: Vec<String> = (0..64_i64)
            .map(|i| format!("({}, {}.5)", chunk * 64 + i + 1, chunk * 64 + i))
            .collect();
        core.execute_insert_sql(
            &ctx(),
            &format!("INSERT INTO b (id, d) VALUES {}", values.join(", ")),
        )
        .expect("seed b");
    }
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.d = a.d)";

    // 内側 640 行の走査だけでは確認点が 1024 回に届かず、2 回目の `poll` は残余評価の
    // 内側ループ（比較ごとの確認点）でのみ到達する。
    let seen = Rc::new(Cell::new(0_u32));
    let s2 = Rc::clone(&seen);
    let _s = InterruptScope::install(Box::new(move || {
        s2.set(s2.get() + 1);
        (s2.get() >= 2).then_some(InterruptReason::ClientGone)
    }));
    let e = run(&core, sql).expect_err("interrupted");
    assert_eq!(e.wire_code(), "57014");
}
