//! 旧形式の台帳エントリ・分割実行ジョブ記録が、行識別の rowid／主キー基準への移行
//! （Issue #2196。ビヘイビア: TABLE-23 (4)(5)・RECOVER-10〜12）の後も従来どおり読め、
//! 再送判定（一致は `23505`・不一致は `22023`）が変わらないことを固定する回帰テスト。
//!
//! 旧バイナリが書いたのと同じバイト列（台帳 v1／v2／v3・ジョブ記録 v1）を表へ直接注入し、
//! `Storage` を開き直してから SQL 表層で再送する。内容照合ハッシュは本番の書き込み経路で
//! 一度記録させた値を使う（ハッシュ自体のバイト列不変は `content_hash` の golden テストが担う）。
//! テナントは 2 つ使い、他テナントの同一 `operation_id`・同一 id が判定に影響しないこと
//! （RLS-9）も確認する。呼び出し元は `recovery.rs` の `#[cfg(test)]` 宣言のみ。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::ledger::OP_LEDGER_TABLE;
use crate::recovery::partitioned_job::{PARTITIONED_JOB_ACTIVE_TABLE, PARTITIONED_JOB_TABLE};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;
use std::path::Path;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn exec_ok(core: &EngineCore, tenant: &str, sql: &str) {
    if let Err(code) = exec(core, tenant, sql) {
        panic!("{sql} must succeed, got {code}");
    }
}

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    // 旧バイナリが書いた台帳・ジョブ記録は従来方式（`id` 列値＝rowid の生値）の表に対するものなので、
    // 表は Rust API（常に従来方式）で作る。SQL の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を
    // 作るため使えない（TABLE-23・Issue #2199）。
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("tag", ColumnType::Text, true),
                    ColumnDef::new("n", ColumnType::BigInt, true),
                ],
            ))
            .expect("create table");
    }
    (path, guard)
}

fn ledger_value(storage: &Storage, tenant: &str, op: &str) -> Vec<u8> {
    let read = storage.db().begin_read().expect("read");
    let t = read.open_table(OP_LEDGER_TABLE).expect("ledger");
    let v = t.get((tenant, "docs", op)).expect("get").expect("entry");
    v.value().to_vec()
}

fn put_ledger_raw(storage: &Storage, tenant: &str, op: &str, value: &[u8]) {
    let w = storage.db().begin_write().expect("write");
    {
        let mut t = w.open_table(OP_LEDGER_TABLE).expect("ledger");
        t.insert((tenant, "docs", op), value).expect("insert");
    }
    w.commit().expect("commit");
}

fn insert_sql(id: u64, tag: &str, op: &str) -> String {
    format!("INSERT INTO docs (id, tag, n) VALUES ({id}, '{tag}', {id}) USING OPERATION_ID '{op}'")
}

/// 台帳 v2・v3・v1 の旧エントリへの再送判定が、再オープン後も従来どおりであること。
#[test]
fn table23_legacy_ledger_entries_keep_resend_semantics_after_reopen() {
    let (path, _g) = setup("t23-compat-ledger");
    {
        let core = open_core(&path);
        for (id, op) in [(1u64, "op-v2"), (2, "op-v3"), (3, "op-v1")] {
            exec_ok(&core, "ta", &insert_sql(id, "a", op));
        }
        let storage = core.storage_for_test();
        let digest_of = |op: &str| -> Vec<u8> {
            ledger_value(storage, "ta", op)
                .get(1..33)
                .expect("digest")
                .to_vec()
        };
        // 旧形式を直接再現する（v2: [2][digest]、v3: [3][digest][i64 LE]、v1: [1]）。
        let mut v2 = vec![2u8];
        v2.extend_from_slice(&digest_of("op-v2"));
        put_ledger_raw(storage, "ta", "op-v2", &v2);
        let mut v3 = vec![3u8];
        v3.extend_from_slice(&digest_of("op-v3"));
        v3.extend_from_slice(&0i64.to_le_bytes());
        put_ledger_raw(storage, "ta", "op-v3", &v3);
        put_ledger_raw(storage, "ta", "op-v1", &[1u8]);
    }
    let core = open_core(&path);
    // v2: 同一内容は 23505、内容違いは 22023。
    assert_eq!(
        exec(&core, "ta", &insert_sql(1, "a", "op-v2")).expect_err("dup"),
        "23505"
    );
    assert_eq!(
        exec(&core, "ta", &insert_sql(1, "b", "op-v2")).expect_err("mismatch"),
        "22023"
    );
    // v3: 同一内容は 23505。
    assert_eq!(
        exec(&core, "ta", &insert_sql(2, "a", "op-v3")).expect_err("dup"),
        "23505"
    );
    // v1（内容ハッシュを持たない旧エントリ）は内容照合できないため 22023。
    assert_eq!(
        exec(&core, "ta", &insert_sql(3, "a", "op-v1")).expect_err("legacy"),
        "22023"
    );
    // 他テナントの同一 operation_id・同一 id は判定に影響しない（RLS-9）。
    exec_ok(&core, "tb", &insert_sql(1, "a", "op-v2"));
}

/// 分割実行ジョブ記録 v1 の旧バイト列（カーソル＝rowid 生値）から、再送で続きだけが適用される。
#[test]
fn table23_legacy_partitioned_job_record_resumes_after_cursor() {
    const JOB_SQL: &str =
        "DELETE FROM docs WHERE tag = 'x' USING OPERATION_ID 'job-c' PARTITIONED CHUNK 2";
    // 本番経路で一度完了させ、台帳に残った内容ハッシュ（ジョブ記録と同一）を採取する。
    let digest: Vec<u8> = {
        let (path, _g) = setup("t23-compat-job-digest");
        let core = open_core(&path);
        exec_ok(&core, "ta", &insert_sql(1, "x", "seed-d"));
        exec_ok(&core, "ta", JOB_SQL);
        ledger_value(core.storage_for_test(), "ta", "job-c")
            .get(1..33)
            .expect("digest")
            .to_vec()
    };
    let (path, _g) = setup("t23-compat-job");
    {
        let core = open_core(&path);
        // ta: 1〜6 が対象。うち 1〜4 は旧バイナリで適用済み（カーソル 4・処理済み 4）の想定。
        for id in 5..=6u64 {
            exec_ok(&core, "ta", &insert_sql(id, "x", &format!("seed-a{id}")));
        }
        // カーソルより前の行（4 以下）は残っていない状態を作る（5 以降だけ存在）。
        // tb: 同じ id・同じタグの行は他テナントなので影響されない。
        for id in 5..=6u64 {
            exec_ok(&core, "tb", &insert_sql(id, "x", &format!("seed-b{id}")));
        }
        let storage = core.storage_for_test();
        let mut rec = vec![1u8, 1];
        rec.extend_from_slice(&digest);
        rec.extend_from_slice(&4u64.to_le_bytes());
        rec.extend_from_slice(&4u64.to_le_bytes());
        let w = storage.db().begin_write().expect("write");
        {
            let mut t = w.open_table(PARTITIONED_JOB_TABLE).expect("job");
            t.insert(("ta", "docs", "job-c"), rec.as_slice())
                .expect("insert job");
            let mut a = w.open_table(PARTITIONED_JOB_ACTIVE_TABLE).expect("active");
            a.insert(("ta", "docs", "job-c"), [1u8].as_slice())
                .expect("insert active");
        }
        w.commit().expect("commit");
    }
    let core = open_core(&path);
    // 異なる述語での再送は 22023（内容違い）。
    assert_eq!(
        exec(
            &core,
            "ta",
            "DELETE FROM docs WHERE tag = 'y' USING OPERATION_ID 'job-c' PARTITIONED CHUNK 2"
        )
        .expect_err("mismatch"),
        "22023"
    );
    // 同一文の再送は完了する。
    exec_ok(&core, "ta", JOB_SQL);
    // 完了後の再送は重複（23505）。
    assert_eq!(exec(&core, "ta", JOB_SQL).expect_err("dup"), "23505");
    // ta の対象行は消え、tb の同 id 行は残る。
    let remaining = |tenant: &str| -> usize {
        match exec(&core, tenant, "SELECT COUNT(*) FROM docs") {
            Ok(SqlOutcome::Query(q)) => match q.rows.first().and_then(|r| r.cells.first()) {
                Some(crate::sql::exec::Cell::Integer(n)) => usize::try_from(*n).expect("count"),
                other => panic!("unexpected cell {other:?}"),
            },
            other => panic!("unexpected outcome {other:?}"),
        }
    };
    assert_eq!(remaining("ta"), 0);
    assert_eq!(remaining("tb"), 2);
}
