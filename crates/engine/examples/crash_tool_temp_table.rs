//! Issue #1685（TABLE-44 (2)・TASK 系列は親 #1680）の一時表・異常終了後残骸検査用ツール。
//! `crash_tool_unique_index.rs` と役割・使い方の形は同じで、`scripts/crash_test_temp_table.sh`
//! から `write` をバックグラウンド起動され SIGKILL される想定、続けて `verify` で再オープン後に
//! 「一時表の痕跡が永続側に一切残っていないこと」を検証される想定で作られている。
//!
//! 一時表は接続ごとのインメモリ `Storage` に置かれ、永続 DB ファイルにも `TMPDIR` にも
//! 触れない設計である。この性質は「将来、一時ストアをディスクへ spill する変更」や「文を誤って
//! 永続コアへ振り分ける変更」で壊れうるため、プロセスを SIGKILL で落としても残骸が無いことを
//! CI で固定する。engine 内の呼び出し元は `EngineCore::execute_sql_in_session` /
//! `execute_sql_in_txn` / `execute_insert_sql`（SQL 表層）で、本ツール自身は production 経路のみ使う。
//!
//! サブコマンド:
//! - `write <db_path>`: 永続表 `docs (code TEXT)` に `perm-{id}` を `BATCH` 件ずつ 1 文でコミットし
//!   続ける（`COMMITTED batch=<seq> rows=<total>` を stdout へ出す。スクリプトの開始同期点）。
//!   並行して一時セッションを複数生かしたままにする（T1: tenant-a の一時表 `docs`〔永続表を隠す〕・
//!   `scratch_tmp`・一時索引、T2: tenant-b の同名 `scratch_tmp`、T3: `ON COMMIT DELETE ROWS`）。
//!   T3 は明示トランザクション内で一時表と永続表へ同時に書き、kill がトランザクション中に当たりうる
//!   ようにする。
//! - `verify <db_path>`: 次を fail-closed で検証し、`RESULT ok=<bool> rows=<n>` を出す。
//!   1. `Storage` の永続カタログに `docs` だけがあり、一時索引が無い
//!   2. redb の生テーブル名に一時表・一時索引由来の名前が無い
//!   3. 永続表 `docs` の行が 0 起点で連続し、総数が `BATCH` の倍数で、全 `code` が `perm-` 始まり
//!      （一時表の書き込みが永続表へ漏れていれば `tmp-` 行が現れる）
//!   4. DB ファイルの親ディレクトリに DB ファイル以外の要素が無い（ツール自身は他の物を作らない）
//!   5. 新しいセッションから両テナントで `scratch_tmp` が 42P01
//!   6. 新しいセッションで同名の `CREATE TEMP TABLE` が成功し、空である（失敗時の出力は件数と表名のみ）
//! - `self-check <workdir>`: 負の対照。健全な DB では `verify` 相当が成功し、「永続表
//!   `scratch_tmp` が作られた DB」と「永続表 `docs` に `tmp-` 行が入った DB」では失敗することを
//!   確かめる（何も無いから通るだけのテストと区別するため。失敗の検出に成功すれば exit 0）。

use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use redb::ReadableDatabase;

const TENANT_A: &str = "crash-tool-temp-tenant-a";
const TENANT_B: &str = "crash-tool-temp-tenant-b";
/// 1 `INSERT` 文（＝1 write トランザクション）で書き込む永続行数。
const BATCH: u64 = 10;
/// `SELECT` の `LIMIT` 上限（SQL 表層の既存上限）。達したら fail-closed に拒否する。
const SCAN_LIMIT: u64 = 10_000;
/// write の自走上限バッチ数（安全弁）。
const MAX_BATCHES: u64 = 500_000;
/// 一時表の行を消す周期（バッチ数）。一時表の総バイト数上限へ達しないようにする。
const TRUNCATE_EVERY: u64 = 20;
/// 一時表・一時索引の名前（永続側に現れてはならない名前）。
const SCRATCH_TABLE: &str = "scratch_tmp";
const SCRATCH_INDEX: &str = "scratch_tmp_code_idx";

fn main() {
    let mut args = std::env::args().skip(1);
    let (sub, path) = match (args.next(), args.next()) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!(
                "usage: crash_tool_temp_table <write|verify> <db_path> | self-check <workdir>"
            );
            std::process::exit(2);
        }
    };
    let exit_code = match sub.as_str() {
        "write" => run_write(&path),
        "verify" => run_verify(&path),
        "self-check" => run_self_check(&path),
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(exit_code);
}

fn ctx(tenant: &str) -> Result<PolicyContext, String> {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn open_core(path: &str) -> Result<EngineCore, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::Ledgered),
    )
}

/// 永続表 `docs` を用意する（既存 DB の再オープンでは重複エラーを許容する）。
fn ensure_persistent_docs(core: &EngineCore) -> Result<(), String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    match core.execute_sql_in_session(
        &ctx(TENANT_A)?,
        &mut session,
        "CREATE TABLE docs (code TEXT)",
    ) {
        Ok(_) => Ok(()),
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => Ok(()),
        Err(e) => Err(format!("create table failed: {e}")),
    }
}

fn exec(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> Result<(), String> {
    core.execute_sql_in_session(&ctx(tenant)?, s, sql)
        .map(|_| ())
        .map_err(|e| format!("{} failed ({}): {e}", sql_head(sql), e.wire_code()))
}

/// エラー文言へ入れる SQL の先頭（行データを含みうる長い文を丸ごと出さない）。
fn sql_head(sql: &str) -> String {
    sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

fn scan_ids_and_codes(core: &EngineCore, table: &str) -> Result<Vec<(u64, String)>, String> {
    let result = core
        .execute_sql(
            &ctx(TENANT_A)?,
            &format!("SELECT code FROM {table} LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("scan failed: {e}"))?;
    if result.rows.len() as u64 >= SCAN_LIMIT {
        return Err(format!(
            "row count reached this tool's scan limit ({SCAN_LIMIT}); reduce sets/iterations"
        ));
    }
    Ok(result
        .rows
        .iter()
        .map(|row| {
            let code = row
                .cells
                .first()
                .map(|c| format!("{c:?}"))
                .unwrap_or_default();
            (row.id, code)
        })
        .collect())
}

fn run_write(path: &str) -> i32 {
    match write_inner(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

fn write_inner(path: &str) -> Result<(), String> {
    let core = open_core(path)?;
    ensure_persistent_docs(&core)?;
    let existing = scan_ids_and_codes(&core, "docs")?;
    let mut next_id = existing
        .iter()
        .map(|(id, _)| *id)
        .max()
        .map_or(0, |m| m + 1);
    if !next_id.is_multiple_of(BATCH) {
        return Err(format!(
            "resumed row id {next_id} is not a multiple of BATCH ({BATCH}); a partial batch must \
             not exist"
        ));
    }

    // 一時セッションは生かしたままにする（プロセスごと SIGKILL されるまで保持）。
    let mut t1 = SessionState::default();
    let mut t2 = SessionState::default();
    let mut t3 = SessionState::default();
    exec(
        &core,
        TENANT_A,
        &mut t1,
        "CREATE TEMP TABLE docs (code TEXT UNIQUE)",
    )?;
    exec(
        &core,
        TENANT_A,
        &mut t1,
        "CREATE TEMP TABLE scratch_tmp (code TEXT, n INTEGER)",
    )?;
    exec(
        &core,
        TENANT_A,
        &mut t1,
        "CREATE INDEX scratch_tmp_code_idx ON scratch_tmp (code)",
    )?;
    exec(
        &core,
        TENANT_B,
        &mut t2,
        "CREATE TEMP TABLE scratch_tmp (code TEXT, n INTEGER)",
    )?;
    exec(
        &core,
        TENANT_A,
        &mut t3,
        "CREATE TEMP TABLE scratch_tmp (code TEXT, n INTEGER) ON COMMIT DELETE ROWS",
    )?;
    let ctx_a = ctx(TENANT_A)?;
    let mut txn = core.new_session_transaction();

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for seq in 0..MAX_BATCHES {
        // 一時側の自動コミット書き込み（T1 は永続表を隠す一時 `docs` へも書く。漏れれば verify が検出）。
        exec(
            &core,
            TENANT_A,
            &mut t1,
            &format!("INSERT INTO docs (id, code) VALUES ({seq}, 'tmp-{seq}-docs')"),
        )?;
        exec(
            &core,
            TENANT_A,
            &mut t1,
            &format!("INSERT INTO scratch_tmp (id, code, n) VALUES ({seq}, 'tmp-{seq}-a', {seq})"),
        )?;
        exec(
            &core,
            TENANT_B,
            &mut t2,
            &format!("INSERT INTO scratch_tmp (id, code, n) VALUES ({seq}, 'tmp-{seq}-b', {seq})"),
        )?;

        // T3 の明示トランザクション内で、一時表への書き込みと永続表の 1 バッチを同時にコミットする。
        // BEGIN から COMMIT までの間（スロットリング中を含む）に kill が当たると、永続バッチは
        // 丸ごと無かったことになる（部分バッチ・一時表の痕跡のいずれも残らない）。トランザクションは
        // 永続側の write ロックを保持するため、別経路の永続書き込みとは並行させない。
        let batch_seq = next_id / BATCH;
        let values: Vec<String> = (0..BATCH)
            .map(|offset| {
                let id = next_id + offset;
                format!("({id}, 'perm-{id}')")
            })
            .collect();
        let perm_sql = format!(
            "INSERT INTO docs (id, code) VALUES {} USING OPERATION_ID 'op-docs-{batch_seq}'",
            values.join(",")
        );
        let scratch_sql =
            format!("INSERT INTO scratch_tmp (id, code, n) VALUES ({seq}, 'tmp-{seq}-c', {seq})");
        for sql in ["BEGIN", scratch_sql.as_str(), perm_sql.as_str()] {
            core.execute_sql_in_txn(&ctx_a, &mut t3, &mut txn, sql)
                .map_err(|e| format!("{} failed ({}): {e}", sql_head(sql), e.wire_code()))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        core.execute_sql_in_txn(&ctx_a, &mut t3, &mut txn, "COMMIT")
            .map_err(|e| format!("COMMIT failed: {e}"))?;
        next_id = next_id
            .checked_add(BATCH)
            .ok_or_else(|| "row id overflow".to_string())?;

        // 進捗行は COMMIT 成功後にだけ出す（スクリプトの開始同期点・acked 行数の根拠）。
        writeln!(handle, "COMMITTED batch={batch_seq} rows={next_id}")
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;

        if seq % TRUNCATE_EVERY == TRUNCATE_EVERY - 1 {
            exec(&core, TENANT_A, &mut t1, "TRUNCATE docs")?;
            exec(&core, TENANT_A, &mut t1, "TRUNCATE scratch_tmp")?;
            exec(&core, TENANT_B, &mut t2, "TRUNCATE scratch_tmp")?;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

fn run_verify(path: &str) -> i32 {
    match verify_inner(path) {
        Ok(rows) => {
            println!("RESULT ok=true rows={rows}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

/// 一時表由来の名前か（永続側に現れてはならない）。
fn is_temp_artifact(name: &str) -> bool {
    name.contains(SCRATCH_TABLE) || name.contains(SCRATCH_INDEX)
}

fn verify_inner(path: &str) -> Result<u64, String> {
    // 1. 永続カタログ。`docs` だけがあり、一時索引・一時表が無い。
    {
        let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
        check_catalog(&storage)?;
    }

    // 2. redb の生テーブル名（カタログを経由しない残骸も拾う）。Storage を閉じてから開く。
    {
        let db = redb::Database::open(path).map_err(|e| format!("raw open failed: {e}"))?;
        let read = db
            .begin_read()
            .map_err(|e| format!("raw read failed: {e}"))?;
        let tables = read
            .list_tables()
            .map_err(|e| format!("raw list failed: {e}"))?;
        let mut count = 0usize;
        for handle in tables {
            count += 1;
            if is_temp_artifact(redb::TableHandle::name(&handle)) {
                return Err("raw redb table derived from a temp object exists".to_string());
            }
        }
        if count == 0 {
            return Err("raw redb has no tables (vacuous)".to_string());
        }
    }

    // 3・5・6. 行・不可視・再作成。
    let rows;
    {
        let core = open_core(path)?;
        rows = verify_docs_rows(&core)?;

        for tenant in [TENANT_A, TENANT_B] {
            let mut fresh = SessionState::default();
            match core.execute_sql_in_session(
                &ctx(tenant)?,
                &mut fresh,
                "SELECT code FROM scratch_tmp LIMIT 1",
            ) {
                Err(e) if e.wire_code() == "42P01" => {}
                Err(e) => {
                    return Err(format!(
                        "scratch_tmp lookup gave {} (want 42P01)",
                        e.wire_code()
                    ))
                }
                Ok(_) => return Err("scratch_tmp is visible from a fresh session".to_string()),
            }
        }
        let mut fresh = SessionState::default();
        exec(
            &core,
            TENANT_A,
            &mut fresh,
            "CREATE TEMP TABLE scratch_tmp (code TEXT, n INTEGER)",
        )?;
        match core
            .execute_sql_in_session(
                &ctx(TENANT_A)?,
                &mut fresh,
                "SELECT COUNT(*) FROM scratch_tmp",
            )
            .map_err(|e| format!("count failed: {e}"))?
        {
            SqlOutcome::Query(r)
                if format!("{:?}", r.rows.first().map(|x| &x.cells)) == "Some([Integer(0)])" => {}
            _ => return Err("recreated scratch_tmp is not empty".to_string()),
        }
    }

    // 6 の後にもう一度カタログを確かめる（作成が永続へ漏れていない）。
    {
        let storage = Storage::open(path).map_err(|e| format!("reopen failed: {e}"))?;
        check_catalog(&storage)?;
    }

    // 4. データディレクトリには DB ファイルだけが存在する。
    check_data_dir(path)?;
    Ok(rows)
}

fn check_catalog(storage: &Storage) -> Result<(), String> {
    let tables = storage
        .list_tables()
        .map_err(|e| format!("list tables failed: {e}"))?;
    if tables != vec!["docs".to_string()] {
        return Err(format!(
            "persistent tables are not exactly [docs]: count={}",
            tables.len()
        ));
    }
    let indexes = storage
        .list_indexes()
        .map_err(|e| format!("list indexes failed: {e}"))?;
    if indexes
        .iter()
        .any(|i| is_temp_artifact(&i.name) || is_temp_artifact(&i.table))
    {
        return Err("persistent catalog has a temp index".to_string());
    }
    Ok(())
}

fn verify_docs_rows(core: &EngineCore) -> Result<u64, String> {
    let mut rows = scan_ids_and_codes(core, "docs")?;
    rows.sort_unstable_by_key(|(id, _)| *id);
    for (expected, (actual, code)) in rows.iter().enumerate() {
        if *actual != expected as u64 {
            return Err(format!(
                "id gap or disorder: expected={expected} actual={actual}"
            ));
        }
        if !code.starts_with("Text(\"perm-") {
            return Err(format!(
                "persistent docs row id={actual} is not a perm row (temp write leaked?)"
            ));
        }
    }
    let total = rows.len() as u64;
    if total == 0 {
        return Err("no rows found".to_string());
    }
    if !total.is_multiple_of(BATCH) {
        return Err(format!(
            "row count {total} is not a multiple of BATCH ({BATCH})"
        ));
    }
    Ok(total)
}

fn check_data_dir(path: &str) -> Result<(), String> {
    let db = std::path::Path::new(path);
    let dir = db
        .parent()
        .ok_or_else(|| "db path has no parent directory".to_string())?;
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir failed: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read_dir entry failed: {e}"))?;
    if entries.len() != 1 || entries[0].path() != db {
        return Err(format!(
            "data directory has {} entries (want only the db file)",
            entries.len()
        ));
    }
    Ok(())
}

// ---- 負の対照 -----------------------------------------------------------------------------

fn run_self_check(workdir: &str) -> i32 {
    match self_check_inner(workdir) {
        Ok(()) => {
            println!("SELF-CHECK ok");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK failed: {reason}");
            1
        }
    }
}

/// `dir/db/crash_tool.redb` に永続表 `docs`（1 バッチ）を持つ DB を作り、`mutate` で壊す。
fn build_db(
    workdir: &str,
    name: &str,
    mutate: impl Fn(&EngineCore) -> Result<(), String>,
) -> Result<String, String> {
    let dir = std::path::Path::new(workdir).join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir failed: {e}"))?;
    let path = dir.join("crash_tool.redb");
    let path = path.to_str().ok_or("non-utf8 path")?.to_string();
    {
        let core = open_core(&path)?;
        ensure_persistent_docs(&core)?;
        let values: Vec<String> = (0..BATCH).map(|i| format!("({i}, 'perm-{i}')")).collect();
        core.execute_insert_sql(
            &ctx(TENANT_A)?,
            &format!(
                "INSERT INTO docs (id, code) VALUES {} USING OPERATION_ID 'op-docs-0'",
                values.join(",")
            ),
        )
        .map_err(|e| format!("seed insert failed: {e}"))?;
        mutate(&core)?;
    }
    Ok(path)
}

fn self_check_inner(workdir: &str) -> Result<(), String> {
    let healthy = build_db(workdir, "healthy", |_| Ok(()))?;
    verify_inner(&healthy).map_err(|r| format!("healthy db was rejected: {r}"))?;

    let leaked_table = build_db(workdir, "leaked-table", |core| {
        let mut s = SessionState::default();
        s.allow_ddl();
        exec(
            core,
            TENANT_A,
            &mut s,
            "CREATE TABLE scratch_tmp (code TEXT)",
        )
    })?;
    if verify_inner(&leaked_table).is_ok() {
        return Err("a persistent scratch_tmp table was not detected".to_string());
    }

    let leaked_row = build_db(workdir, "leaked-row", |core| {
        core.execute_insert_sql(
            &ctx(TENANT_A)?,
            "INSERT INTO docs (id, code) VALUES (10, 'tmp-leak') USING OPERATION_ID 'op-docs-leak'",
        )
        .map(|_| ())
        .map_err(|e| format!("leak insert failed: {e}"))
    })?;
    // 1 行だけの漏れは BATCH の倍数性でも検出されるが、理由が行の中身である点を確かめる。
    match verify_inner(&leaked_row) {
        Ok(_) => return Err("a tmp- row in persistent docs was not detected".to_string()),
        Err(reason)
            if reason.contains("temp write leaked") || reason.contains("multiple of BATCH") => {}
        Err(reason) => return Err(format!("unexpected rejection reason: {reason}")),
    }

    let stray_file = build_db(workdir, "stray-file", |_| Ok(()))?;
    let stray = std::path::Path::new(&stray_file)
        .parent()
        .ok_or("no parent")?
        .join("spill.tmp");
    std::fs::write(&stray, b"x").map_err(|e| format!("stray write failed: {e}"))?;
    if verify_inner(&stray_file).is_ok() {
        return Err("a stray file next to the db was not detected".to_string());
    }
    Ok(())
}
