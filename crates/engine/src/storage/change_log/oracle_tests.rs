//! 変更ログのオラクル試験の場面集（D1 の層 3・RECOVER-14 のポインタ。Issue #3069）。
//!
//! 変更ログを有効にした `EngineCore`／`Storage` に SQL と公開 API の書き込みを広く流す。
//! 各 commit のレコードは `CaptureSession::prepare` の中で `table_image::capture` の実差分と突き合わせ
//! 済み（不一致は panic。`oracle::verify_record`）。さらに本ファイルの [`finish`] が、有効化した時点の
//! 全表の内容へ、記録されたレコードを LSN 順にすべて適用した結果が最終状態と一致すること
//! （符号化・LSN の連鎖・正規化を通した再生の検証）と、レコードの数が世代の進みと一致すること
//! （commit の取りこぼしがない）を確かめる。刈り込み（#2167）を挟む場面では、刈り込まれた区間を
//! ベースのスナップショットで補う [`finish_from`] を使う（Issue #3095）。

use super::oracle::{self, Model};
use super::{decode_record, ChangeRecord, CHANGE_LOG_TABLE};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::{ReadableDatabase, ReadableTable};

struct Scenario {
    path: std::path::PathBuf,
    _guard: CleanupGuard,
    start: Model,
    start_gen: u64,
}

fn dump(storage: &Storage) -> Model {
    let txn = storage.db.begin_write().expect("begin_write");
    let model = oracle::dump_writer(&txn).expect("dump");
    txn.abort().expect("abort");
    model
}

fn records(storage: &Storage) -> Vec<ChangeRecord> {
    let read = storage.db.begin_read().expect("read");
    let table = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Vec::new(),
        Err(e) => panic!("open change_log: {e}"),
    };
    table
        .iter()
        .expect("iter")
        .map(|e| {
            let (_, v) = e.expect("entry");
            decode_record(v.value(), usize::MAX).expect("decode")
        })
        .collect()
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("tenant")
}

/// 変更ログを有効にした `Storage` を開く（`prepare` を通すと oracle が各 commit を検証する）。
fn begin(label: &str) -> (Scenario, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    storage.enable_change_log().expect("enable");
    let start = dump(&storage);
    let start_gen = storage.current_generation().expect("generation");
    (
        Scenario {
            path,
            _guard: guard,
            start,
            start_gen,
        },
        storage,
    )
}

/// 場面の後始末。再度開いて、レコードの数・連鎖・再生結果を検証する。
fn finish(s: Scenario) -> usize {
    let storage = Storage::open(&s.path).expect("reopen");
    let recs = records(&storage);
    let gen_now = storage.current_generation().expect("generation");
    let lsns: Vec<u64> = recs.iter().map(|r| r.header.lsn).collect();
    assert_eq!(
        u64::try_from(recs.len()).expect("len"),
        gen_now - s.start_gen,
        "one record per generation step: start={} now={gen_now} lsns={lsns:?}",
        s.start_gen
    );
    let replayed = oracle::replay(s.start.clone(), s.start_gen, &recs).expect("replay");
    let actual = dump(&storage);
    if let Some(diff) = oracle::diff_models(&actual, &replayed) {
        panic!("replay differs from the final state: {diff}");
    }
    recs.len()
}

/// LSN B 時点のベース（全表の内容）と B を返す。刈り込み後の再生で、刈り込まれた区間の代わりになる。
fn base_point(storage: &Storage) -> (Model, u64) {
    (
        dump(storage),
        storage.current_generation().expect("generation"),
    )
}

/// 刈り込み後の場面の後始末。再度開いて、ベース以後のレコードだけを再生して実表と突き合わせ、
/// 適用したレコード数を返す（場面が空振りでないことの確認用）。
fn finish_from(s: Scenario, base: Model, base_lsn: u64) -> usize {
    let storage = Storage::open(&s.path).expect("reopen");
    oracle::verify_log_replay_from(&storage, base, base_lsn).expect("replay from base");
    records(&storage)
        .iter()
        .filter(|r| r.header.lsn > base_lsn)
        .count()
}

/// `change_log` 表に残っている行の LSN。
fn row_lsns(storage: &Storage) -> Vec<u64> {
    records(storage).iter().map(|r| r.header.lsn).collect()
}

/// 書き出し先に使える（他者が書けない）一時ディレクトリ。umask に依らず 0755 にする。
fn private_dir(label: &str) -> crate::test_util::temp_db::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = crate::test_util::temp_db::TempDir::new(label);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    d
}

/// SQL を 1 文ずつ流す。すべて成功しなければならない（vacuous な場面にしない）。
fn run_sql(core: &EngineCore, tenant: &str, session: &mut SessionState, steps: &[&str]) {
    let c = ctx(tenant);
    // 明示トランザクションを含む場面だけセッションのトランザクション入口を使う（トリガーなど一部の
    // 文は明示トランザクションの外の入口でだけ実行できる）。
    let explicit = steps.iter().any(|s| s.eq_ignore_ascii_case("BEGIN"));
    let mut txn = core.new_session_transaction();
    for sql in steps {
        let result = if explicit {
            core.execute_sql_in_txn(&c, session, &mut txn, sql)
        } else {
            core.execute_sql_in_session(&c, session, sql)
        };
        result.unwrap_or_else(|e| panic!("{sql}: {:?}", e.wire_code()));
    }
}

fn scenario(label: &str, tenant: &str, steps: &[&str]) -> usize {
    scenario_with(label, |core| {
        let mut session = SessionState::default();
        session.allow_ddl();
        run_sql(core, tenant, &mut session, steps);
    })
}

/// 任意の本体（複数のセッション・テナントを使う場面）を流して検証する。
fn scenario_with(label: &str, body: impl FnOnce(&EngineCore)) -> usize {
    let (s, storage) = begin(label);
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        body(&core);
    }
    finish(s)
}

#[test]
fn ddl_and_dml_lifecycle() {
    let n = scenario(
        "oracle-ddl-dml",
        "tenant-a",
        &[
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(2), note TEXT, n INTEGER)",
            "INSERT INTO ev (id, embedding, note, n) VALUES (1, '[0.1,0.2]', 'a', 1), (2, '[0.3,0.4]', 'b', 2), (3, '[0.5,0.6]', 'c', 3)",
            "UPDATE ev SET note = 'z', n = n + 10 WHERE id = 2",
            "DELETE FROM ev WHERE id = 1",
            "INSERT INTO ev (id, embedding, note, n) VALUES (3, '[0.7,0.8]', 'dup', 9) ON CONFLICT (id) DO UPDATE SET note = EXCLUDED.note",
            "ALTER TABLE ev ADD COLUMN extra TEXT",
            "UPDATE ev SET extra = 'x'",
            "ALTER TABLE ev RENAME COLUMN extra TO extra2",
            "ALTER TABLE ev ALTER COLUMN n TYPE BIGINT",
            "CREATE INDEX ev_note ON ev (note)",
            "CREATE UNIQUE INDEX ev_n ON ev (n)",
            "INSERT INTO ev (id, embedding, note, n) VALUES (10, '[0.1,0.1]', 'q', 100)",
            "DROP INDEX ev_note",
            "ALTER TABLE ev RENAME TO ev2",
            "UPDATE ev2 SET note = 'renamed' WHERE id = 10",
            "ALTER TABLE ev2 RENAME TO ev",
            "TRUNCATE ev",
            "INSERT INTO ev (id, embedding, note, n) VALUES (1, '[0.1,0.2]', 'again', 1)",
            "DROP TABLE ev",
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
            "INSERT INTO ev (id, embedding) VALUES (1, '[0.1,0.2,0.3]')",
            "DROP TABLE ev",
        ],
    );
    assert!(n >= 15, "records: {n}");
}

#[test]
fn views_and_materialized_views() {
    scenario(
        "oracle-views",
        "tenant-a",
        &[
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT, embedding VECTOR(2))",
            "INSERT INTO docs (id, body, embedding) VALUES (1, 'one', '[0.1,0.2]'), (2, 'two', '[0.3,0.4]')",
            "CREATE VIEW v_docs AS SELECT id, body FROM docs",
            "CREATE MATERIALIZED VIEW mv AS SELECT id, body FROM docs",
            "INSERT INTO docs (id, body, embedding) VALUES (3, 'three', '[0.5,0.6]')",
            "REFRESH MATERIALIZED VIEW mv",
            "DELETE FROM docs WHERE id = 1",
            "REFRESH MATERIALIZED VIEW mv",
            "DROP MATERIALIZED VIEW mv",
            "DROP VIEW v_docs",
            "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
            "DROP MATERIALIZED VIEW mv",
            "CREATE MATERIALIZED VIEW mv3 AS SELECT id FROM docs WITH NO DATA",
            "REFRESH MATERIALIZED VIEW mv3",
            "DROP MATERIALIZED VIEW mv3",
        ],
    );
}

#[test]
fn sequences_and_identity() {
    scenario(
        "oracle-seq",
        "tenant-a",
        &[
            "CREATE SEQUENCE s",
            "CREATE TABLE t (id BIGINT PRIMARY KEY, k BIGINT DEFAULT nextval('s'), v VECTOR(2))",
            "INSERT INTO t (id, v) VALUES (1, '[0.1,0.2]')",
            "INSERT INTO t (id, v) VALUES (2, '[0.3,0.4]')",
            "SELECT setval('s', 100)",
            "INSERT INTO t (id, v) VALUES (3, '[0.5,0.6]')",
            "TRUNCATE t RESTART IDENTITY",
            "INSERT INTO t (id, v) VALUES (4, '[0.7,0.8]')",
            "DROP TABLE t",
            "DROP SEQUENCE s",
        ],
    );
}

#[test]
fn gin_index_maintenance() {
    scenario(
        "oracle-gin",
        "tenant-a",
        &[
            "CREATE TABLE g (id BIGINT PRIMARY KEY, doc JSONB)",
            "INSERT INTO g (id, doc) VALUES (1, '{\"a\": 1, \"b\": [1,2]}'), (2, '{\"a\": 2}')",
            "CREATE INDEX g_ops ON g USING gin (doc jsonb_ops)",
            "CREATE INDEX g_path ON g USING gin (doc jsonb_path_ops)",
            "INSERT INTO g (id, doc) VALUES (3, '{\"c\": {\"d\": 4}}')",
            "UPDATE g SET doc = '{\"a\": 9}' WHERE id = 1",
            "DELETE FROM g WHERE id = 2",
            "DROP INDEX g_ops",
            "INSERT INTO g (id, doc) VALUES (4, '{\"z\": 1}')",
            "DROP TABLE g",
        ],
    );
}

/// トリガ関数の呼び出し口（本番の結線は未整備のため、`trg` への INSERT 発火で `audit` に 1 行書く
/// テスト用の実装を注入する）。トリガ内の書き込みも同じ commit のレコードに載ることを確かめる。
struct AuditInvoker;

impl crate::trigger::call::TriggerFunctionInvoker for AuditInvoker {
    fn invoke(
        &self,
        c: &crate::trigger::call::TriggerCall<'_>,
    ) -> Result<crate::trigger::call::TriggerOutcome, crate::sql::allowlist::SqlSurfaceError> {
        use crate::row_codec::Value;
        if c.table == "trg" {
            c.writes.insert_row(
                "audit",
                None,
                &[
                    Value::Vector(vec![0.0, 0.0]),
                    Value::Text("fired".to_string()),
                    Value::BigInt(1),
                ],
            )?;
        }
        Ok(crate::trigger::call::TriggerOutcome::Proceed(
            c.new_row.to_vec(),
        ))
    }
}

#[test]
fn triggers_and_functions() {
    use crate::catalog::{ColumnDef, ColumnType, RowIdentity, TableSchema};
    let schema = |name: &str| {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        )
    };
    let (s, storage) = begin("oracle-trigger");
    storage.create_table(&schema("trg")).expect("trg");
    storage
        .create_table(&schema("audit").with_row_identity(RowIdentity::Allocated))
        .expect("audit");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_trigger_invoker_for_test(std::sync::Arc::new(AuditInvoker));
        let mut session = SessionState::default();
        session.allow_ddl();
        run_sql(
            &core,
            "tenant-a",
            &mut session,
            &[
                "CREATE FUNCTION trg_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
                "CREATE TRIGGER ta AFTER INSERT ON trg FOR EACH ROW EXECUTE FUNCTION trg_fn()",
                "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) USING OPERATION_ID 'tr-0'",
                "INSERT INTO trg (id, embedding, a, n) VALUES (2, '[0.0,1.0]', 'y', 2) USING OPERATION_ID 'tr-1'",
                "DROP TRIGGER ta ON trg",
                "INSERT INTO trg (id, embedding, a, n) VALUES (3, '[0.5,0.5]', 'z', 3) USING OPERATION_ID 'tr-2'",
                "CREATE FUNCTION add1(x int) RETURNS int BEGIN ATOMIC SELECT x + 1; END",
                "DROP FUNCTION add1",
                "DROP FUNCTION trg_fn",
            ],
        );
    }
    finish(s);
}

#[test]
fn explicit_transactions_and_savepoints() {
    scenario(
        "oracle-txn",
        "tenant-a",
        &[
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2), n INTEGER)",
            "BEGIN",
            "INSERT INTO t (id, v, n) VALUES (1, '[0.1,0.2]', 1)",
            "SAVEPOINT a",
            "INSERT INTO t (id, v, n) VALUES (2, '[0.3,0.4]', 2)",
            "UPDATE t SET n = 50 WHERE id = 1",
            "ROLLBACK TO a",
            "INSERT INTO t (id, v, n) VALUES (3, '[0.5,0.6]', 3)",
            "SAVEPOINT b",
            "DELETE FROM t WHERE id = 1",
            "UPDATE t SET n = 7 WHERE id = 3",
            "ROLLBACK TO b",
            "SAVEPOINT c",
            "DELETE FROM t WHERE id = 3",
            "RELEASE SAVEPOINT c",
            "COMMIT",
            "BEGIN",
            "DELETE FROM t",
            "ROLLBACK",
            "BEGIN",
            "INSERT INTO t (id, v, n) VALUES (8, '[0.8,0.8]', 8)",
            "UPDATE t SET n = n + 1",
            "COMMIT",
        ],
    );
}

#[test]
fn temp_tables_do_not_leak_into_the_log() {
    scenario(
        "oracle-temp",
        "tenant-a",
        &[
            "CREATE TABLE p (id BIGINT PRIMARY KEY, v VECTOR(2))",
            "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, n INTEGER)",
            "INSERT INTO tt (id, n) VALUES (1, 1), (2, 2)",
            "INSERT INTO p (id, v) VALUES (1, '[0.1,0.2]')",
            "UPDATE tt SET n = 5",
            "DROP TABLE tt",
            "CREATE TEMP TABLE d (id BIGINT PRIMARY KEY, a TEXT) ON COMMIT DELETE ROWS",
            "INSERT INTO d (id, a) VALUES (1, 'x')",
        ],
    );
}

#[test]
fn schemas_roles_policies_and_types() {
    scenario_with("oracle-catalog", |core| {
        let mut op = SessionState::default();
        op.allow_ddl();
        assert!(op.bind_resolution_tenant("tenant-a"));
        run_sql(core, "tenant-a", &mut op, &["CREATE ROLE obs"]);
        let mut owner = SessionState::default();
        owner.bind_role_ids(1, 1);
        assert!(owner.bind_resolution_tenant("tenant-a"));
        owner.allow_ddl();
        run_sql(
            core,
            "tenant-a",
            &mut owner,
            &[
                "CREATE SCHEMA sa",
                "CREATE TABLE sa.t (id BIGINT PRIMARY KEY, v VECTOR(2), owner TEXT)",
                "INSERT INTO sa.t (id, v, owner) VALUES (1, '[0.1,0.2]', 'a')",
                "CREATE TYPE mood AS ENUM ('sad', 'ok')",
                "CREATE POLICY p ON sa.t FOR SELECT USING (owner = 'a')",
                "ALTER TABLE sa.t ENABLE ROW LEVEL SECURITY",
                "CREATE TABLE mine (id BIGINT PRIMARY KEY, n INTEGER)",
                "GRANT SELECT ON mine TO obs",
                "REVOKE SELECT ON mine FROM obs",
                "DROP POLICY p ON sa.t",
                "DROP TYPE mood",
                "DROP TABLE sa.t",
                "DROP SCHEMA sa",
                "DROP TABLE mine",
            ],
        );
        run_sql(core, "tenant-a", &mut op, &["DROP ROLE obs"]);
    });
}

#[test]
fn partitioned_dml_commits_per_chunk() {
    scenario(
        "oracle-partitioned",
        "tenant-a",
        &[
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, n INTEGER, v VECTOR(2))",
            "INSERT INTO docs (id, n, v) VALUES (1, 1, '[0.1,0.2]'), (2, 2, '[0.3,0.4]'), (3, 3, '[0.5,0.6]')",
            "UPDATE docs SET n = 0 WHERE n > 0 USING OPERATION_ID 'pp1' PARTITIONED CHUNK 1",
            "DELETE FROM docs WHERE n = 0 USING OPERATION_ID 'pp2' PARTITIONED CHUNK 2",
        ],
    );
}

#[test]
fn operation_ledger_and_multiple_tenants() {
    let (s, storage) = begin("oracle-tenants");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = SessionState::default();
        session.allow_ddl();
        run_sql(
            &core,
            "tenant-a",
            &mut session,
            &[
                "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2))",
                "INSERT INTO t (id, v) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'op-1'",
                "UPDATE t SET v = '[0.5,0.5]' WHERE id = 1 USING OPERATION_ID 'op-2'",
            ],
        );
        run_sql(
            &core,
            "tenant-b",
            &mut session,
            &[
                "INSERT INTO t (id, v) VALUES (2, '[0.3,0.4]') USING OPERATION_ID 'op-1'",
                "DELETE FROM t WHERE id = 2 USING OPERATION_ID 'op-3'",
            ],
        );
    }
    finish(s);
}

#[test]
fn storage_api_put_batch_and_batch_write_txn() {
    use crate::storage::RowInput;
    let (s, storage) = begin("oracle-storage-api");
    let row = |tenant: &'static str, e: &'static [f32]| RowInput {
        tenant_id: tenant,
        visibility: Visibility::Public,
        embedding: e,
        metadata: &[],
    };
    storage.put(1, &row("tenant-a", &[0.1, 0.2])).expect("put");
    storage
        .put_batch(&[
            (2, row("tenant-a", &[0.3, 0.4])),
            (3, row("tenant-b", &[0.5, 0.6])),
        ])
        .expect("put_batch");
    storage
        .put(1, &row("tenant-a", &[0.9, 0.9]))
        .expect("overwrite");
    {
        let mut batch = storage.begin_batch_write().expect("begin batch");
        batch.put(10, &row("tenant-a", &[0.1, 0.1])).expect("put");
        batch.put(11, &row("tenant-a", &[0.2, 0.2])).expect("put");
        batch.log_batch(1).expect("log batch");
        batch.put(12, &row("tenant-a", &[0.3, 0.3])).expect("put");
        batch.log_batch(2).expect("log batch");
        batch.commit().expect("commit");
    }
    {
        let mut txn = storage.begin_write().expect("begin");
        txn.put(20, &row("tenant-a", &[0.4, 0.4])).expect("put");
        txn.commit().expect("commit");
    }
    drop(storage);
    finish(s);
}

// ---- オラクル自身の検出力（層 2 が見逃す書き込みを層 3 が捕まえる）----

/// 行数が変わらない更新を捕捉なしで行うと、層 2（行数の増減）は検出できないが、オラクルは
/// 実差分とレコードの不一致として panic する。
#[test]
fn the_oracle_catches_an_uncaptured_in_place_update_that_layer_two_misses() {
    use super::CapturingTable;
    use crate::storage::commit_log;
    let (_s, storage) = begin("oracle-negative");
    let physical = crate::catalog::user_rows_physical_name("ev");
    let def = crate::catalog::user_rows_table_def(&physical);
    let write = |raw_update: bool| {
        let txn = storage.begin_write_txn().expect("txn");
        crate::catalog::bump_table_generation_in_txn(&txn, "ev").expect("bump");
        commit_log::note_table(&txn, "ev");
        {
            let mut t = CapturingTable::open(&txn, def).expect("open");
            t.insert(("tenant", 3u64), b"new".as_slice())
                .expect("insert");
        }
        if raw_update {
            // 捕捉を通さない更新（行数は変わらない）。
            let mut t = txn.open_table(def).expect("raw open");
            t.insert(("tenant", 1u64), b"sneaky".as_slice())
                .expect("update");
        }
        crate::recovery::commit_boundary::commit(txn)
    };
    // 基底に 2 行置く（捕捉つき）。
    let txn = storage.begin_write_txn().expect("txn");
    crate::catalog::bump_table_generation_in_txn(&txn, "ev").expect("bump");
    commit_log::note_table(&txn, "ev");
    {
        let mut t = CapturingTable::open(&txn, def).expect("open");
        t.insert(("tenant", 1u64), b"a".as_slice()).expect("insert");
        t.insert(("tenant", 2u64), b"b".as_slice()).expect("insert");
    }
    crate::recovery::commit_boundary::commit(txn).expect("seed");
    write(false).expect("captured write passes the oracle");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(true)));
    let payload = outcome.expect_err("the oracle must reject the uncaptured update");
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(msg.contains("change log oracle mismatch"), "{msg}");
}

// ---- 有効化の境界（D8）----

/// 有効にした DB を有効にしないまま開くと書き込みを拒否し（読み取りは通す）、有効にして開き直すと
/// 記録が連続して続く。拒否は世代もレコードも動かさない。
#[test]
fn a_database_with_a_change_log_refuses_writes_unless_it_is_enabled() {
    use crate::storage::RowInput;
    let (s, storage) = begin("oracle-sticky");
    let row = |e: &'static [f32]| RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: e,
        metadata: &[],
    };
    storage.put(1, &row(&[0.1, 0.2])).expect("put");
    drop(storage);

    // 有効にしない起動: 読めるが書けない。
    let plain = Storage::open(&s.path).expect("reopen");
    assert!(plain.change_log_required());
    assert!(plain.get("tenant-a", 1).is_ok(), "reads are unaffected");
    let before = plain.current_generation().expect("gen");
    let err = plain.put(2, &row(&[0.3, 0.4])).expect_err("write refused");
    assert!(matches!(
        err,
        crate::storage::StorageError::CommitFootprintUnrecordable
    ));
    assert_eq!(plain.current_generation().expect("gen"), before);
    drop(plain);

    // 有効にした起動: 記録が続く。
    let mut again = Storage::open(&s.path).expect("reopen");
    again.enable_change_log().expect("re-enable");
    assert!(!again.change_log_required());
    again.put(2, &row(&[0.3, 0.4])).expect("put");
    drop(again);
    assert_eq!(finish(s), 2);
}

/// 無効のまま世代が進んだ区間（LSN の欠落）がある DB は、有効化を拒否し、書き込みも拒否されたまま。
#[test]
fn re_enabling_refuses_a_gap_in_the_log() {
    use crate::storage::RowInput;
    let (s, storage) = begin("oracle-gap");
    let row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2],
        metadata: &[],
    };
    storage.put(1, &row).expect("put");
    drop(storage);

    // 変更ログを知らない書き手（旧版・別ツール）が世代を進めた状況を、素の redb で再現する。
    {
        let raw = Storage::open(&s.path).expect("open");
        let txn = raw.db.begin_write().expect("begin");
        crate::storage::prepare_generation_bump(&txn).expect("bump");
        txn.commit().expect("commit");
    }

    let mut again = Storage::open(&s.path).expect("reopen");
    assert!(again.enable_change_log().is_err(), "the gap is refused");
    assert!(again.change_log_required(), "writes stay refused");
    assert!(again.put(2, &row).is_err());
}

/// 初回の有効化は現在の世代を起点にし、レコードを生まない。旧形式の表の移行は、有効にした DB では
/// open ではなく有効化の直後（記録される状態）で行われ、LSN の欠落を作らない。
#[test]
fn legacy_table_migration_is_recorded_when_the_log_is_enabled() {
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    let (s, storage) = begin("oracle-migration");
    storage
        .create_table(&TableSchema::new(
            "legacy",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("legacy-format table");
    let gen_after_create = storage.current_generation().expect("gen");
    drop(storage);

    let plain = Storage::open(&s.path).expect("reopen");
    assert_eq!(
        plain.current_generation().expect("gen"),
        gen_after_create,
        "open must not commit on a database with a change log"
    );
    drop(plain);

    let mut again = Storage::open(&s.path).expect("reopen");
    again.enable_change_log().expect("enable");
    assert!(
        again.current_generation().expect("gen") > gen_after_create,
        "the deferred migration commits once the log is enabled"
    );
    drop(again);
    // 作成とその後の移行がどちらもレコードになり、再生結果が一致する。
    assert!(finish(s) >= 2);
}

// ---- 層 2 が呼ぶ世代の差分検出（突き合わせ実装が点引きと同じ結果を返す）----

/// 確定側の世代表と書き込み側を整列して突き合わせる実装が、表ごとの点引きと同じ集合を返す
/// （更新・新規・変化なし・確定側にだけ在る表・世代表が未作成の確定側）。
#[test]
fn changed_table_generations_matches_the_per_table_lookup() {
    use super::CaptureWriteTxn as _;
    use crate::catalog::{bump_table_generation_in_txn, table_generation_in_txn};
    let reference = |txn: &redb::WriteTransaction, base: &redb::ReadTransaction| {
        let names = ["a", "b", "c", "d", "e", "zz", "A", "ab"];
        let mut out: Vec<String> = names
            .iter()
            .filter(|n| {
                table_generation_in_txn(txn, n).expect("write side")
                    != table_generation_in_txn(base, n).expect("base side")
            })
            .map(|n| (*n).to_string())
            .collect();
        out.sort();
        out
    };
    // SQL-38 の commit 記録（別機構）の足跡も組にして積む。
    let bump = |txn: &crate::storage::GatedWriteTxn, n: &str| {
        bump_table_generation_in_txn(txn, n).expect("bump");
        crate::storage::commit_log::note_table(txn, n);
    };
    let (_s, storage) = begin("oracle-changed-gens");

    // 確定側に世代表が無い状態。
    {
        let base = storage.db.begin_read().expect("base");
        let txn = storage.begin_write_txn().expect("txn");
        bump(&txn, "b");
        bump(&txn, "a");
        let mut got =
            crate::catalog::changed_table_generations_in_write_txn(&txn, &base).expect("changed");
        got.sort();
        assert_eq!(got, reference(&txn, &base));
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
        crate::recovery::commit_boundary::commit(txn).expect("commit");
    }
    // 確定側に a・b・c・e が在る状態から、b を更新・d を新規・c を削除・a と e は変えない。
    {
        let txn = storage.begin_write_txn().expect("txn");
        for n in ["c", "e", "zz"] {
            bump(&txn, n);
        }
        crate::recovery::commit_boundary::commit(txn).expect("commit");
    }
    let base = storage.db.begin_read().expect("base");
    let txn = storage.begin_write_txn().expect("txn");
    bump(&txn, "b");
    bump(&txn, "d");
    bump(&txn, "ab");
    {
        let mut t = (*txn)
            .open_table_logged(crate::catalog::TABLE_GENERATION_TABLE)
            .expect("open");
        t.remove("c").expect("remove");
    }
    let mut got =
        crate::catalog::changed_table_generations_in_write_txn(&txn, &base).expect("changed");
    got.sort();
    // 削除された表（確定側にだけ在る）は書き込み側の走査に現れない: 従来の点引きの意味と同じ。
    let mut expected = reference(&txn, &base);
    expected.retain(|n| n != "c");
    assert_eq!(got, expected);
    assert_eq!(
        got,
        vec!["ab".to_string(), "b".to_string(), "d".to_string()]
    );
    drop(txn);
}

// ---- 有効にした DB をフラグなしで開いたときの拒否の経路 ----

/// SQL の INSERT・DDL と `put_batch` は `54000` 系で拒否され（durable な書き込みも世代の進みも無い）、
/// SELECT と一貫スナップショットの書き出し（`backup` の本体）は通る。
#[test]
fn every_write_path_is_refused_without_the_flag_but_reads_and_snapshots_pass() {
    use crate::storage::RowInput;
    let (s, storage) = begin("oracle-refusal");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = SessionState::default();
        session.allow_ddl();
        run_sql(
            &core,
            "tenant-a",
            &mut session,
            &[
                "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2))",
                "INSERT INTO t (id, v) VALUES (1, '[0.1,0.2]')",
            ],
        );
    }

    // 有効にしない起動: put_batch は拒否される。
    let plain = Storage::open(&s.path).expect("reopen");
    let before = plain.current_generation().expect("gen");
    let row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2],
        metadata: &[],
    };
    let err = plain.put_batch(&[(5, row)]).expect_err("put_batch refused");
    assert!(matches!(
        err,
        crate::storage::StorageError::CommitFootprintUnrecordable
    ));
    // 出力先の親は共有でない（0700）ディレクトリにする（Issue #3135）。
    let snap_dir = private_dir("oracle-refusal-snapshot");
    let snap = snap_dir.path().join("snap.redb");
    plain.export_snapshot(&snap).expect("snapshot is a read");
    drop(plain);

    // SQL: INSERT と DDL は 54000、SELECT は通る。
    let core = EngineCore::from_storage(
        Storage::open(&s.path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    let c = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        "INSERT INTO t (id, v) VALUES (2, '[0.3,0.4]')",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, v VECTOR(2))",
    ] {
        let err = core
            .execute_sql_in_session(&c, &mut session, sql)
            .expect_err("refused");
        assert_eq!(err.wire_code(), "54000", "{sql}");
    }
    core.execute_sql_in_session(&c, &mut session, "SELECT id FROM t")
        .expect("reads are unaffected");
    drop(core);
    let storage = Storage::open(&s.path).expect("reopen");
    assert_eq!(storage.current_generation().expect("gen"), before);
    drop(storage);
    assert_eq!(finish(s) as u64, before);
}

// ---- shape の符号化そのものの検査（記録・突き合わせ・再生と別の読み方）----

/// 記録・突き合わせ・再生は同じ `table_image` の読み取り（shape）を共有するため、符号化そのものの誤りは
/// 3 つとも同じ向きにずれて見逃しうる。行ストアの実体を素の redb で直接読み、キーと値のバイト列が
/// 再生結果と一致することを確かめる。
#[test]
fn replayed_row_store_matches_a_direct_raw_redb_read() {
    let (s, storage) = begin("oracle-raw-bytes");
    {
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = SessionState::default();
        session.allow_ddl();
        run_sql(
            &core,
            "tenant-a",
            &mut session,
            &[
                "CREATE TABLE ev (id BIGINT PRIMARY KEY, v VECTOR(2), note TEXT)",
                "INSERT INTO ev (id, v, note) VALUES (1, '[0.1,0.2]', 'a'), (2, '[0.3,0.4]', 'b')",
                "UPDATE ev SET note = 'z' WHERE id = 2",
                "DELETE FROM ev WHERE id = 1",
                "INSERT INTO ev (id, v, note) VALUES (3, '[0.5,0.6]', 'c')",
            ],
        );
    }
    let storage = Storage::open(&s.path).expect("reopen");
    let recs = records(&storage);
    drop(storage);
    let replayed = oracle::replay(s.start.clone(), s.start_gen, &recs).expect("replay");
    let physical = crate::catalog::user_rows_physical_name("ev");
    let (_, model_rows) = replayed.get(&physical).expect("row store replayed");

    let db = redb::Database::open(&s.path).expect("raw open");
    let read = db.begin_read().expect("read");
    let table = read
        .open_table(crate::catalog::user_rows_table_def(&physical))
        .expect("raw typed open");
    let mut raw: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = std::collections::BTreeMap::new();
    for entry in table.iter().expect("iter") {
        let (k, v) = entry.expect("entry");
        let key = <(&str, u64) as redb::Value>::as_bytes(&k.value()).to_vec();
        raw.insert(key, v.value().to_vec());
    }
    assert_eq!(raw.len(), 2, "rows 2 and 3 remain");
    assert_eq!(&raw, model_rows, "the replayed bytes equal the raw bytes");
    drop(table);
    drop(read);
    drop(db);
    finish(s);
}
// ---- 刈り込みを挟む場面（Issue #3095。ベースのスナップショットから再生する） ----

/// 刈り込み場面の結果。
struct Pruned {
    applied: usize,
    pruned: u64,
    first_lsn: Option<u64>,
    base_lsn: u64,
    old_path_replay_failed: bool,
}

/// W1（作成と INSERT）→ ベース B → W2（4 件）→ アーカイブ（保持 `retain` 件）→ W3（4 件）→ 後始末。
fn pruned_scenario(label: &str, retain: u64) -> Pruned {
    let (s, storage) = begin(label);
    let archive = private_dir("oracle-prune-arc");
    let mut session = SessionState::default();
    session.allow_ddl();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, v VECTOR(2), note TEXT)",
            "INSERT INTO ev (id, v, note) VALUES (1, '[0.1,0.2]', 'a')",
            "INSERT INTO ev (id, v, note) VALUES (2, '[0.3,0.4]', 'b')",
        ],
    );
    let (base, base_lsn) = base_point(core.storage_for_test());
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "UPDATE ev SET note = 'u' WHERE id = 1",
            "DELETE FROM ev WHERE id = 2",
            "INSERT INTO ev (id, v, note) VALUES (3, '[0.5,0.6]', 'c')",
            "INSERT INTO ev (id, v, note) VALUES (4, '[0.7,0.8]', 'd')",
        ],
    );
    let mut opts = crate::storage::ArchiveOptions::new(archive.path());
    opts.retain_records = Some(retain);
    let report = core
        .storage_for_test()
        .archive_change_log(&opts)
        .expect("archive");
    let first_lsn = row_lsns(core.storage_for_test()).first().copied();
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "ALTER TABLE ev ADD COLUMN extra TEXT",
            "CREATE TABLE tmp2 (id BIGINT PRIMARY KEY)",
            "INSERT INTO tmp2 (id) VALUES (1)",
            "DROP TABLE tmp2",
        ],
    );
    let old_path_replay_failed = oracle::replay(
        s.start.clone(),
        s.start_gen,
        &records(core.storage_for_test()),
    )
    .is_err();
    drop(core);
    let applied = finish_from(s, base, base_lsn);
    Pruned {
        applied,
        pruned: report.pruned_records,
        first_lsn,
        base_lsn,
        old_path_replay_failed,
    }
}

/// 保持 6 件でベースと保持行が重なる場面。従来の全行再生は成り立たず、ベースからの再生が通る。
#[test]
fn replay_from_base_after_pruning_with_overlap() {
    let p = pruned_scenario("oracle-prune-overlap", 6);
    assert!(p.pruned > 0, "something was pruned");
    assert!(p.first_lsn.is_some_and(|l| l <= p.base_lsn));
    assert!(
        p.old_path_replay_failed,
        "the full-log replay no longer holds"
    );
    assert_eq!(p.applied, 4 + 4, "W2 (4) and W3 (4) after the base");
}

/// 検出力の負例。刈り込みがベースを越えると、ベースからの再生は拒否される（黙って通さない）。
#[test]
fn replay_from_base_rejects_pruning_beyond_the_base() {
    let (s, storage) = begin("oracle-prune-beyond");
    let archive = private_dir("oracle-prune-beyond-arc");
    let mut session = SessionState::default();
    session.allow_ddl();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, v VECTOR(2))",
            "INSERT INTO ev (id, v) VALUES (1, '[0.1,0.2]')",
        ],
    );
    let (base, base_lsn) = base_point(core.storage_for_test());
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "INSERT INTO ev (id, v) VALUES (2, '[0.3,0.4]')",
            "INSERT INTO ev (id, v) VALUES (3, '[0.5,0.6]')",
        ],
    );
    let mut opts = crate::storage::ArchiveOptions::new(archive.path());
    // 保持 1 件: B + 1 も刈り込まれ、先頭は B + 2 になる。
    opts.retain_records = Some(1);
    core.storage_for_test()
        .archive_change_log(&opts)
        .expect("archive");
    assert_eq!(row_lsns(core.storage_for_test()), vec![base_lsn + 2]);
    let err = oracle::verify_log_replay_from(core.storage_for_test(), base.clone(), base_lsn)
        .expect_err("a gap after the base must be rejected");
    assert!(err.contains("pruned beyond the base"), "{err}");
    // ベースの内容が B と食い違う場合も一致しない。
    let mut wrong = base;
    for (_, rows) in wrong.values_mut() {
        rows.clear();
    }
    assert!(
        oracle::verify_log_replay_from(core.storage_for_test(), wrong, base_lsn + 1).is_err(),
        "a wrong base must not pass"
    );
    drop(core);
    drop(s);
}

/// 全件刈り込み（保持 0 件）と利用者の commit を交互に 2 往復挟んでも通る。
#[test]
fn replay_from_base_after_full_pruning_repeated() {
    let (s, storage) = begin("oracle-prune-full");
    let archive = private_dir("oracle-prune-full-arc");
    let mut session = SessionState::default();
    session.allow_ddl();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &[
            "CREATE TABLE ev (id BIGINT PRIMARY KEY, v VECTOR(2), note TEXT)",
            "INSERT INTO ev (id, v, note) VALUES (1, '[0.1,0.2]', 'a')",
            "UPDATE ev SET note = 'u' WHERE id = 1",
        ],
    );
    let (base, base_lsn) = base_point(core.storage_for_test());
    let mut opts = crate::storage::ArchiveOptions::new(archive.path());
    opts.retain_records = Some(0);
    let r = core
        .storage_for_test()
        .archive_change_log(&opts)
        .expect("archive");
    assert!(r.pruned_records > 0);
    assert!(row_lsns(core.storage_for_test()).is_empty());
    oracle::verify_log_replay_from(core.storage_for_test(), base.clone(), base_lsn)
        .expect("an empty log replays to the base");
    // 刈り込みのたびに、刈り込む直前の状態を新しいベースにする（古いベースだと間の区間が欠ける）。
    let (mut base, mut base_lsn) = (base, base_lsn);
    for i in 0..2 {
        let sql = format!(
            "INSERT INTO ev (id, v, note) VALUES ({}, '[0.5,0.6]', 'w')",
            10 + i
        );
        run_sql(&core, "tenant-a", &mut session, &[&sql]);
        (base, base_lsn) = base_point(core.storage_for_test());
        core.storage_for_test()
            .archive_change_log(&opts)
            .expect("archive again");
    }
    run_sql(
        &core,
        "tenant-a",
        &mut session,
        &["INSERT INTO ev (id, v, note) VALUES (99, '[0.1,0.1]', 'z')"],
    );
    drop(core);
    let applied = finish_from(s, base, base_lsn);
    assert!(applied >= 1, "records after the base: {applied}");
}
