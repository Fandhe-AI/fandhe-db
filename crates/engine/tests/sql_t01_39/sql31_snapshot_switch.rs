//! 明示トランザクションの読み取りスナップショットの切り替え（Issue #2403。親 #2396・TASK-333）の
//! 結合テスト。ポインタ: SQL-19・SQL-31（設計記録は `docs/design/explicit-transaction.md` 1a-2）。
//!
//! 確認する契約: (1) writer 取得前の読み取りは `BEGIN` 時点のスナップショットを見て、他セッションの
//! commit で変わらない、(2) writer 取得後の述語評価・読み取りは取得時点の最新の確定状態に自分の
//! 未 commit 変更を加えた状態を見る、(3) テナント境界は変わらない。#2405 の `40001` 判定が入っても
//! 壊れないよう、取得前に読む表（`docs`）と他セッションが書く表（`docs2`）を分ける。
//! 同一スレッドで順に実行し、時間計測に依存しない。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE docs (n BIGINT)");
    ok(&core, &sys, "CREATE TABLE docs2 (n BIGINT)");
    (core, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn rows(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Query(r) => {
            let mut v: Vec<String> = r
                .rows
                .iter()
                .map(|row| format!("{:?}", row.cells))
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed in the transaction, got {e:?}"))
    }

    fn rows(&mut self, sql: &str) -> Vec<String> {
        rows(self.ok(sql))
    }
}

fn seed_docs(core: &EngineCore, tenant: &str) {
    ok(
        core,
        &ctx(tenant),
        "INSERT INTO docs (id, n) VALUES (1, 10), (2, 20), (3, 30) USING OPERATION_ID 'seed'",
    );
}

/// AC1: 取得後の述語形 UPDATE／DELETE は、取得前に他セッションが commit した行を対象に含む。
#[test]
fn predicate_dml_after_writer_acquire_sees_rows_committed_before_acquire() {
    let (core, _g) = new_core("sql31-snap-ac1");
    seed_docs(&core, "tenant-a");
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    a.rows("SELECT id FROM docs LIMIT 100");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO docs2 (id, n) VALUES (7, 1), (8, 5) USING OPERATION_ID 'b1'",
    );
    a.ok("UPDATE docs2 SET n = 2 WHERE n = 1 USING OPERATION_ID 'a-up'");
    a.ok("DELETE FROM docs2 WHERE n = 5 USING OPERATION_ID 'a-del'");
    a.ok("COMMIT");
    let after = rows(ok(&core, &ctx("tenant-a"), "SELECT id, n FROM docs2"));
    assert_eq!(
        after.len(),
        1,
        "DELETE must have removed the concurrently committed row: {after:?}"
    );
    assert!(
        after[0].contains('2'),
        "UPDATE must have changed n to 2: {after:?}"
    );
}

/// AC1 補足: 取得後の読み取りは取得時点の最新状態と自分の変更を見る。
#[test]
fn reads_after_writer_acquire_see_latest_committed_and_own_writes() {
    let (core, _g) = new_core("sql31-snap-after");
    seed_docs(&core, "tenant-a");
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    assert_eq!(a.rows("SELECT id FROM docs LIMIT 100").len(), 3);
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO docs2 (id, n) VALUES (7, 1) USING OPERATION_ID 'b1'",
    );
    a.ok("INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'a1'");
    assert_eq!(a.rows("SELECT id FROM docs2 LIMIT 100").len(), 1);
    assert_eq!(a.rows("SELECT id FROM docs LIMIT 100").len(), 4);
    a.ok("COMMIT");
}

/// AC2: 取得前の読み取り（Scan・集計・集合演算・カーソル・COPY TO）は他セッションの
/// commit で変わらず、`COMMIT` の後は最新が見える。
#[test]
fn reads_before_writer_acquire_keep_begin_time_snapshot() {
    let (core, _g) = new_core("sql31-snap-before");
    seed_docs(&core, "tenant-a");
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    let scan = a.rows("SELECT id, n FROM docs LIMIT 100");
    assert_eq!(scan.len(), 3);

    let other = ctx("tenant-a");
    ok(
        &core,
        &other,
        "INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'b-ins'",
    );
    ok(
        &core,
        &other,
        "UPDATE docs SET n = 99 WHERE id = 1 USING OPERATION_ID 'b-up'",
    );
    ok(
        &core,
        &other,
        "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'b-del'",
    );

    assert_eq!(a.rows("SELECT id, n FROM docs LIMIT 100"), scan);
    let count = a.rows("SELECT COUNT(*) FROM docs");
    assert!(count[0].contains('3'), "COUNT must stay at 3: {count:?}");
    assert_eq!(
        a.rows("SELECT id FROM docs UNION ALL SELECT id FROM docs2 LIMIT 100")
            .len(),
        3
    );

    a.ok("DECLARE c CURSOR FOR SELECT id, n FROM docs LIMIT 100");
    match a.ok("FETCH 100 FROM c") {
        SqlOutcome::Fetch(r) => assert_eq!(r.rows.len(), 3),
        other => panic!("unexpected {other:?}"),
    }

    let plan = core
        .begin_copy_in_txn(
            &a.ctx,
            &a.session,
            &mut a.txn,
            "COPY (SELECT id, n FROM docs LIMIT 100) TO STDOUT",
        )
        .expect("copy to");
    match plan {
        CopyPlan::To(_, result, _) => assert_eq!(result.rows.len(), 3),
        other => panic!("unexpected {other:?}"),
    }

    a.ok("COMMIT");
    let latest = rows(ok(&core, &other, "SELECT id, n FROM docs LIMIT 100"));
    assert_ne!(latest, scan);
    assert_eq!(latest.len(), 3);
}

/// 同時 DDL: writer 取得前の `DECLARE`・`COPY TO` の検証は `BEGIN` 時点のカタログで解決する。
/// `BEGIN` 後に他セッションが DROP した表は読め（スナップショットに存在する）、`BEGIN` 後に
/// 作られた表は `42P01` になる（実行の読み取り源とカタログの引き先が食い違わない）。
#[test]
fn declare_and_copy_to_validate_against_begin_snapshot_catalog() {
    let (core, _g) = new_core("sql31-snap-ddl");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE docs3 (n BIGINT)");
    seed_docs(&core, "tenant-a");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO docs3 (id, n) VALUES (1, 10) USING OPERATION_ID 'd3-ins'",
    );
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");

    ok(&core, &sys, "DROP TABLE docs3");
    ok(&core, &sys, "CREATE TABLE docs4 (n BIGINT)");

    a.ok("DECLARE c CURSOR FOR SELECT id, n FROM docs3 LIMIT 100");
    match a.ok("FETCH 100 FROM c") {
        SqlOutcome::Fetch(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("unexpected {other:?}"),
    }
    let plan = core
        .begin_copy_in_txn(
            &a.ctx,
            &a.session,
            &mut a.txn,
            "COPY (SELECT id, n FROM docs3 LIMIT 100) TO STDOUT",
        )
        .expect("copy to of a table dropped after BEGIN");
    match plan {
        CopyPlan::To(_, result, _) => assert_eq!(result.rows.len(), 1),
        other => panic!("unexpected {other:?}"),
    }

    let err = core
        .execute_sql_in_txn(
            &a.ctx,
            &mut a.session,
            &mut a.txn,
            "DECLARE d CURSOR FOR SELECT id, n FROM docs4",
        )
        .expect_err("table created after BEGIN is not in the snapshot");
    assert_eq!(err.wire_code(), "42P01", "{err:?}");
}

/// テナント境界: 取得前の読み取りに他テナントの行は現れず、他テナントの書き込みも待たされない。
#[test]
fn begin_snapshot_reads_do_not_cross_tenants() {
    let (core, _g) = new_core("sql31-snap-tenant");
    seed_docs(&core, "tenant-a");
    ok(
        &core,
        &ctx("tenant-b"),
        "INSERT INTO docs (id, n) VALUES (50, 500) USING OPERATION_ID 'b-seed'",
    );
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    let before = a.rows("SELECT id, n FROM docs LIMIT 100");
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|r| !r.contains("500")));
    ok(
        &core,
        &ctx("tenant-b"),
        "INSERT INTO docs (id, n) VALUES (51, 501) USING OPERATION_ID 'b-more'",
    );
    assert_eq!(a.rows("SELECT id, n FROM docs LIMIT 100"), before);
    a.ok("COMMIT");
}

/// キャッシュ経路: 取得前の SELECT を繰り返しても BEGIN 時点を返し、別セッションは最新を返す。
#[test]
fn begin_snapshot_reads_do_not_poison_or_use_stale_cache() {
    let (core, _g) = new_core("sql31-snap-cache");
    seed_docs(&core, "tenant-a");
    let caller = ctx("tenant-a");
    let q = "SELECT id FROM docs LIMIT 100";
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    assert_eq!(a.rows(q).len(), 3);
    assert_eq!(a.rows(q).len(), 3);
    ok(
        &core,
        &caller,
        "INSERT INTO docs (id, n) VALUES (4, 40) USING OPERATION_ID 'b1'",
    );
    assert_eq!(rows(ok(&core, &caller, q)).len(), 4);
    assert_eq!(a.rows(q).len(), 3);
    assert_eq!(rows(ok(&core, &caller, q)).len(), 4);
    a.ok("COMMIT");
}

/// 通常の SELECT のビュー展開も `BEGIN` 時点のカタログで行う（Issue #2403）。`BEGIN` 後に他セッションが
/// `CREATE OR REPLACE VIEW` で述語だけを変えても、writer 取得前の同一トランザクション内の結果は変わらず、
/// BEGIN 後に作られたビューは見えない（`42P01`）。
#[test]
fn plain_select_expands_view_against_begin_snapshot_catalog() {
    let (core, _g) = new_core("sql31-snap-view");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE tagged (tag TEXT)");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tagged (id, tag) VALUES (1, 'a'), (2, 'b'), (3, 'c') USING OPERATION_ID 'tg'",
    );
    ok(
        &core,
        &sys,
        "CREATE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'a'",
    );
    let before = rows(ok(&core, &ctx("tenant-a"), "SELECT id FROM docs_v"));
    assert_eq!(before.len(), 1);
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");

    ok(
        &core,
        &sys,
        "CREATE OR REPLACE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'b'",
    );
    ok(
        &core,
        &sys,
        "CREATE VIEW docs_v2 AS SELECT id FROM tagged WHERE tag = 'c'",
    );

    assert_eq!(a.rows("SELECT id FROM docs_v LIMIT 100"), before);
    let err = core
        .execute_sql_in_txn(&a.ctx, &mut a.session, &mut a.txn, "SELECT id FROM docs_v2")
        .expect_err("view created after BEGIN is not in the snapshot");
    assert_eq!(err.wire_code(), "42P01", "{err:?}");
}

/// PL/pgSQL 本体内の SELECT も `BEGIN` 時点のカタログでビューを展開する（Issue #2403）。`BEGIN` 後に
/// 他セッションが述語を変えても、関数内 SELECT は直接 SELECT と同じ結果（旧定義 × 旧行）になる。
#[test]
fn plpgsql_body_select_expands_view_against_begin_snapshot_catalog() {
    let (core, _g) = new_core("sql31-snap-plpgsql-view");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE tagged (tag TEXT)");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tagged (id, tag) VALUES (1, 'a'), (2, 'b'), (3, 'b') USING OPERATION_ID 'tg'",
    );
    ok(
        &core,
        &sys,
        "CREATE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'a'",
    );
    ok(&core, &sys, "CREATE TABLE probe (n BIGINT)");
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");

    ok(
        &core,
        &sys,
        "CREATE OR REPLACE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'b'",
    );

    // 本体の SELECT の結果を probe へ書き留める（本体内の読み取りは writer 取得前）。
    a.ok(
        "DO $$ DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs_v; \
         INSERT INTO probe (id, n) VALUES (1, c); END $$",
    );
    a.ok("COMMIT");
    // BEGIN 時点の旧定義（tag = 'a'）× 旧行 = 1 行。新定義 × 旧行なら 2 行になる。
    let probe = rows(ok(&core, &ctx("tenant-a"), "SELECT n FROM probe"));
    assert_eq!(probe.len(), 1, "{probe:?}");
    assert!(
        probe[0].contains("1") && !probe[0].contains("2"),
        "{probe:?}"
    );
}

/// 拡張クエリの Bind も `BEGIN` 時点のカタログでビューを展開する（Issue #2403）。Parse が `BEGIN` 後の
/// 最新カタログで行われても、`bind_prepared_in_txn` は簡易クエリ経路と同じ結果になる。
#[test]
fn extended_bind_expands_view_against_begin_snapshot_catalog() {
    let (core, _g) = new_core("sql31-snap-ext");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE tagged (tag TEXT)");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tagged (id, tag) VALUES (1, 'a'), (2, 'b') USING OPERATION_ID 'tg'",
    );
    ok(
        &core,
        &sys,
        "CREATE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'a'",
    );
    let mut a = Tx::new(&core, "tenant-a");
    let simple_before = rows(ok(&core, &ctx("tenant-a"), "SELECT id FROM docs_v"));
    a.ok("BEGIN");
    ok(
        &core,
        &sys,
        "CREATE OR REPLACE VIEW docs_v AS SELECT id FROM tagged WHERE tag = 'b'",
    );
    let prepared = core
        .parse_sql_prepared("SELECT id FROM docs_v LIMIT 100")
        .expect("prepare");
    let bound = core
        .bind_prepared_in_txn(&a.session, &a.txn, &prepared, &[])
        .expect("bind");
    let out = core
        .execute_parsed_in_txn(&a.ctx, &mut a.session, &mut a.txn, &bound)
        .expect("execute");
    assert_eq!(rows(out), simple_before);
}

/// `EXPLAIN` は writer 取得前でも最新の確定カタログで実行されるため、検証も最新カタログで行う
/// （`BEGIN` 後に作られた表でも `42P01` にならない。Issue #2403）。
#[test]
fn explain_validates_against_latest_catalog_not_begin_snapshot() {
    let (core, _g) = new_core("sql31-snap-explain");
    let sys = ctx("sys");
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    ok(&core, &sys, "CREATE TABLE late_t (n BIGINT)");
    a.ok("EXPLAIN SELECT id FROM late_t");
}

/// 拡張クエリの Describe も `BEGIN` 時点のカタログで結果列を決める（Issue #2403）。BEGIN 後に別セッションが
/// 列型を変えても、Describe の列と Bind 後の Describe・Execute の列が一致する（`ResultTypeChanged` にならない）。
/// パラメータ付き（`describe_prepared_in_txn`）と、Bind 済みの文（`describe_parsed_in_txn`）の両方を確かめる。
#[test]
fn extended_describe_uses_begin_snapshot_column_types() {
    let (core, _g) = new_core("sql31-snap-describe");
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE typed (qty BIGINT)");
    ok(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO typed (id, qty) VALUES (1, 5) USING OPERATION_ID 'dt'",
    );
    let mut a = Tx::new(&core, "tenant-a");
    a.ok("BEGIN");
    // 別セッションが列型を変更して commit する（A の読み取りは writer 取得前）。
    ok(&core, &sys, "ALTER TABLE typed ALTER COLUMN qty TYPE TEXT");

    // パラメータなし: Parse（最新カタログ）→ Bind（BEGIN 時点で再検証）→ Describe → Execute。
    let prepared = core
        .parse_sql_prepared("SELECT qty FROM typed LIMIT 100")
        .expect("prepare");
    let bound = core
        .bind_prepared_in_txn(&a.session, &a.txn, &prepared, &[])
        .expect("bind");
    let described = core
        .describe_parsed_in_txn(&a.session, &a.txn, &bound)
        .expect("describe")
        .expect("columns");
    let SqlOutcome::Query(result) = core
        .execute_parsed_in_txn(&a.ctx, &mut a.session, &mut a.txn, &bound)
        .expect("execute")
    else {
        panic!("expected Query outcome");
    };
    assert_eq!(
        format!("{described:?}"),
        format!("{:?}", result.columns),
        "Describe の列は Execute の列（BEGIN 時点の BIGINT）と一致する"
    );
    assert!(format!("{described:?}").contains("BigInt"), "{described:?}");

    // パラメータ付き: 文の Describe（'S'）・Bind 後の Describe・Execute がすべて同じ列型になる。
    let prepared = core
        .parse_sql_prepared("SELECT qty FROM typed WHERE qty = $1 LIMIT 100")
        .expect("prepare with param");
    let stmt_cols = core
        .describe_prepared_in_txn(&a.session, &a.txn, &prepared)
        .expect("describe prepared")
        .expect("columns");
    let bound = core
        .bind_prepared_in_txn(&a.session, &a.txn, &prepared, &[Some(b"5".to_vec())])
        .expect("bind with param");
    let portal_cols = core
        .describe_parsed_in_txn(&a.session, &a.txn, &bound)
        .expect("describe bound")
        .expect("columns");
    let SqlOutcome::Query(result) = core
        .execute_parsed_in_txn(&a.ctx, &mut a.session, &mut a.txn, &bound)
        .expect("execute with param")
    else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1);
    assert_eq!(format!("{stmt_cols:?}"), format!("{:?}", result.columns));
    assert_eq!(format!("{portal_cols:?}"), format!("{:?}", result.columns));
    a.ok("COMMIT");
}
