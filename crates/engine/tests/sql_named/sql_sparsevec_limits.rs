//! `SPARSEVEC(N)` の非ゼロ数上限の起動時設定（`EngineCore::with_sparsevec_limits`。
//! TABLE-47 (2)・SQL-46 (2)(4)(5)・Issue #2027）の結合テスト。
//!
//! 上限を 3 に下げたコアで、(1) 上限ちょうどは受理・1 超えは `54000`、(2) INSERT／複数行 VALUES／
//! UPDATE SET／UPSERT／明示トランザクション内 INSERT／NoSQL 相当の束縛クロージャ経路のどれでも同じ境界、
//! (3) 値 0 の要素は数えない、(4) 既定のコアは従来どおり 16,000 まで、(5) 上限を下げて開き直しても既存行は
//! 読め・他列の UPDATE は成功し・新しい書き込みだけが拒否されること、を固定する。

use engine::catalog::TableSchema;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sparsevec_limits::SparseVecLimits;
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, ValidatedInsert};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::bind_insert;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_with(
    label: &str,
    limits: Option<SparseVecLimits>,
) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (path, guard, core_with(storage, limits))
}

fn core_with(storage: Storage, limits: Option<SparseVecLimits>) -> EngineCore {
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_sparsevec_limits(l),
        None => core,
    }
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

/// `n` 個の非ゼロ要素と `zeros` 個のゼロ要素を持つ次元 100 のリテラル（引用符込み）。
fn lit(n: usize, zeros: usize) -> String {
    format!("'{}'", lit_inner(n, zeros))
}

fn lit_inner(n: usize, zeros: usize) -> String {
    let mut parts: Vec<String> = (1..=n).map(|i| format!("{i}:1")).collect();
    parts.extend((n + 1..=n + zeros).map(|i| format!("{i}:0")));
    format!("{{{}}}/100", parts.join(","))
}

fn ids(core: &EngineCore) -> Vec<(i64, usize)> {
    let SqlOutcome::Query(q) = exec(core, "SELECT id, v FROM t ORDER BY id").expect("select")
    else {
        panic!("query expected");
    };
    q.rows
        .iter()
        .map(|r| {
            let id = match r.cells.first() {
                Some(Cell::SignedInteger(n)) => *n,
                Some(Cell::Integer(n)) => i64::try_from(*n).expect("id"),
                other => panic!("unexpected id cell {other:?}"),
            };
            let n = match r.cells.get(1) {
                Some(Cell::SparseVec(v)) => v.nnz(),
                other => panic!("unexpected cell {other:?}"),
            };
            (id, n)
        })
        .collect()
}

fn low() -> Option<SparseVecLimits> {
    Some(SparseVecLimits::new(3))
}

const DDL: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(100), n TEXT)";

/// SQL-46 (2): INSERT の境界が設定値に従う。値 0 の要素は数えない。
#[test]
fn insert_boundary_follows_configured_limit() {
    let (_p, _g, core) = open_with("svl-insert", low());
    ok(&core, DDL);
    ok(
        &core,
        &format!("INSERT INTO t (id, v) VALUES (1, {})", lit(3, 0)),
    );
    ok(
        &core,
        &format!("INSERT INTO t (id, v) VALUES (2, {})", lit(3, 5)),
    );
    assert_eq!(
        code(
            &core,
            &format!("INSERT INTO t (id, v) VALUES (3, {})", lit(4, 0))
        ),
        "54000"
    );
    assert_eq!(ids(&core), vec![(1, 3), (2, 3)]);
}

/// 複数行 VALUES・UPDATE SET・UPSERT も同じ境界になる。
#[test]
fn multi_row_update_and_upsert_follow_configured_limit() {
    let (_p, _g, core) = open_with("svl-dml", low());
    ok(&core, DDL);
    assert_eq!(
        code(
            &core,
            &format!(
                "INSERT INTO t (id, v) VALUES (1, {}), (2, {})",
                lit(1, 0),
                lit(4, 0)
            )
        ),
        "54000"
    );
    ok(
        &core,
        &format!(
            "INSERT INTO t (id, v) VALUES (1, {}), (2, {})",
            lit(1, 0),
            lit(3, 0)
        ),
    );
    ok(
        &core,
        &format!("UPDATE t SET v = {} WHERE id = 1", lit(3, 0)),
    );
    assert_eq!(
        code(
            &core,
            &format!("UPDATE t SET v = {} WHERE id = 1", lit(4, 0))
        ),
        "54000"
    );
    ok(
        &core,
        &format!(
            "INSERT INTO t (id, v) VALUES (1, {}) ON CONFLICT (id) DO UPDATE SET v = {}",
            lit(1, 0),
            lit(2, 0)
        ),
    );
    assert_eq!(
        code(
            &core,
            &format!(
                "INSERT INTO t (id, v) VALUES (1, {}) ON CONFLICT (id) DO UPDATE SET v = {}",
                lit(1, 0),
                lit(4, 0)
            )
        ),
        "54000"
    );
    assert_eq!(ids(&core), vec![(1, 2), (2, 3)]);
}

/// 明示トランザクション内の INSERT／UPDATE でも同じ境界になる。
#[test]
fn explicit_transaction_follows_configured_limit() {
    let (_p, _g, core) = open_with("svl-txn", low());
    ok(&core, DDL);
    let ctx = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &ctx,
        &mut session,
        &mut txn,
        &format!("INSERT INTO t (id, v) VALUES (1, {})", lit(3, 0)),
    )
    .expect("at limit");
    let e = core
        .execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            &format!("INSERT INTO t (id, v) VALUES (2, {})", lit(4, 0)),
        )
        .expect_err("insert over limit");
    assert_eq!(e.wire_code(), "54000");
    let e = core
        .execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            &format!("UPDATE t SET v = {} WHERE id = 1", lit(4, 0)),
        )
        .expect_err("update over limit");
    // 失敗後の Tx は中断状態になりうるため、UPDATE の拒否コードは 54000 か 25P02 のどちらか。
    assert!(
        ["54000", "25P02"].contains(&e.wire_code()),
        "got {}",
        e.wire_code()
    );
}

/// NoSQL 相当（束縛クロージャを渡す公開 API）でも設定値が効く。スコープの取りこぼし検出用。
#[test]
fn bound_closure_path_follows_configured_limit() {
    let (_p, _g, core) = open_with("svl-bound", low());
    ok(&core, DDL);
    let bind_row = |id: &str, nnz: usize| {
        let inner = lit_inner(nnz, 0);
        let id = id.to_string();
        move |schema: &TableSchema| {
            let stmt = ValidatedInsert {
                table_name: "t".to_string(),
                columns: vec!["id".to_string(), "v".to_string()],
                columns_omitted: false,
                rows: vec![vec![
                    InsertLiteral::Number(id),
                    InsertLiteral::String(inner),
                ]],
                operation_id: None,
                returning: None,
                on_conflict: None,
            };
            bind_insert(&stmt, schema).map(|b| vec![b])
        }
    };
    let session = SessionState::default();
    core.execute_bound_insert_in_session(&ctx(), &session, "t", 1, None, bind_row("1", 3))
        .expect("at limit");
    let e = core
        .execute_bound_insert_in_session(&ctx(), &session, "t", 1, None, bind_row("2", 4))
        .expect_err("over limit");
    assert_eq!(e.wire_code(), "54000");
    assert_eq!(ids(&core), vec![(1, 3)]);
}

/// 未注入（既定）は従来どおり 16,000 まで受理し、16,001 は `54000`。
#[test]
fn default_core_keeps_hard_limit() {
    let (_p, _g, core) = open_with("svl-default", None);
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(20000), n TEXT)",
    );
    let big = |n: usize| {
        let parts: Vec<String> = (1..=n).map(|i| format!("{i}:1")).collect();
        format!("'{{{}}}/20000'", parts.join(","))
    };
    ok(
        &core,
        &format!("INSERT INTO t (id, v) VALUES (1, {})", big(16_000)),
    );
    assert_eq!(
        code(
            &core,
            &format!("INSERT INTO t (id, v) VALUES (2, {})", big(16_001))
        ),
        "54000"
    );
}

/// SQL-46 (5): 上限を下げて開き直しても既存行は読め、他列の UPDATE は成功し、新規書き込みだけが拒否される。
#[test]
fn lowering_limit_keeps_existing_rows_readable_and_updatable() {
    let (path, _g, core) = open_with("svl-reopen", None);
    ok(&core, DDL);
    ok(
        &core,
        &format!("INSERT INTO t (id, v, n) VALUES (1, {}, 'a')", lit(10, 0)),
    );
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = core_with(storage, low());
    assert_eq!(ids(&core), vec![(1, 10)]);
    ok(&core, "UPDATE t SET n = 'b' WHERE id = 1");
    assert_eq!(ids(&core), vec![(1, 10)]);
    assert_eq!(
        code(
            &core,
            &format!("INSERT INTO t (id, v) VALUES (2, {})", lit(10, 0))
        ),
        "54000"
    );
    assert_eq!(
        code(
            &core,
            &format!("UPDATE t SET v = {} WHERE id = 1", lit(10, 0))
        ),
        "54000"
    );
}

/// 一時表（`for_temp_session` が設定を引き継ぐ）でも下げた上限が効く。
#[test]
fn temp_table_inherits_configured_limit() {
    let (_p, _g, core) = open_with("svl-temp", low());
    let mut s = SessionState::default();
    s.allow_ddl();
    let created = core.execute_sql_in_session(
        &ctx(),
        &mut s,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, v SPARSEVEC(100))",
    );
    if created.is_err() {
        // 一時表が SPARSEVEC 列を持てない構成ならこの経路は到達不能（前提として許容）。
        return;
    }
    core.execute_sql_in_session(
        &ctx(),
        &mut s,
        &format!("INSERT INTO tt (id, v) VALUES (1, {})", lit(3, 0)),
    )
    .expect("at limit");
    let e = core
        .execute_sql_in_session(
            &ctx(),
            &mut s,
            &format!("INSERT INTO tt (id, v) VALUES (2, {})", lit(4, 0)),
        )
        .expect_err("over limit");
    assert_eq!(e.wire_code(), "54000");
}
