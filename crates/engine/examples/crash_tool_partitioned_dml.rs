//! Issue #1131（分割実行 DML の crash 耐性。ポインタ: RECOVER-11・RECOVER-12・RLS-9・
//! PERSIST-1。ADR `docs/design/partitioned-dml.md` 9.3 節）の回帰テスト用ツール。
//!
//! `scripts/crash_test_partitioned_dml.sh` から `write` をバックグラウンド起動して SIGKILL
//! し、`verify` で再オープン後の状態を検証、最後に `finish` で再送完了までを検証する。
//! 既存の crash-test 系（1 トランザクションの部分 commit が 0 件）とは別の、分割実行
//! （チャンクごとに commit する非原子の述語形 UPDATE／DELETE）専用の基準を持つ。
//!
//! 検証基準（ADR 9.3 節）とオラクルの対応:
//! 1. チャンク原子性: 変更集合 D の大きさ == SHOW の処理済み件数（主オラクル）。
//!    `interrupted` なら |D| がチャンク幅の倍数（補助。下記 limits 前提）。
//! 2. 前方一致: D は一致 id（昇順）の先頭部分列と一致する。SHOW はカーソルを公開しないが、
//!    並行書き込みがなく各チャンクが範囲内の一致行をすべて適用するため、
//!    「カーソル以下の一致行の集合」と「一致列の prefix」は同値になる。
//! 3. ちょうど 1 回: `finish` で完了後に DELETE は一致行が全消去・非一致行が全残存し、
//!    返却件数 == 一致行数 == SHOW の件数になる。
//! 4. 台帳エントリ: 完了前は 0 件、完了後は `operation_recorded` が Recorded。
//!
//! 前提: `max_writer_hold` を範囲上限（5000ms）、`scan_budget_rows` を全行数より大きくして
//! いるため、チャンクを締める条件は「適用行数がチャンク幅に達した」だけになる
//! （|D| % CHUNK == 0 の補助オラクルはこの前提に依存する）。
//!
//! サブコマンド（`<mode>` は `delete` または `update`。それ以外は exit 2）:
//! - `init <db> <mode>`: テーブル作成・テナント A/B へ投入。`SEEDED rows=<n>` を出力。
//! - `write <db> <mode>`: 分割実行を発行し進捗を `PROGRESS status=<s> rows=<n>` で出力。
//!   完了後（`DONE`／`ALREADY_COMPLETED`）は kill を待って sleep し続ける（exit 137 の契約を
//!   全反復で同じにする）。
//! - `verify <db> <mode>`: 読み取り専用の検証。`RESULT ok=... status=... rows=...`。
//! - `finish <db> <mode>`: 再送して完了させ、ちょうど 1 回等を検証する。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::PartitionedDmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

/// ジョブを実行するテナント。
const TENANT_A: &str = "crash-pdml-tenant-a";
/// 境界確認用テナント（ジョブを実行しない。行・ジョブ記録・台帳が不変であることを見る）。
const TENANT_B: &str = "crash-pdml-tenant-b";
/// テナント A の行数（偶数 id が一致行。SELECT の LIMIT 上限 10000 未満に保つ）。
const ROWS_A: u64 = 9000;
/// テナント B の行数。
const ROWS_B: u64 = 200;
/// 分割実行のチャンク幅。
const CHUNK: u64 = 2;
/// 投入 1 文あたりの行数（既定の `max_insert_rows` 以下）。
const SEED_BATCH: u64 = 64;
/// SELECT の LIMIT（SQL 表層の上限。到達したら fail-closed で拒否する）。
const SCAN_LIMIT: usize = 10_000;
/// ジョブの operation_id。
const JOB_OP: &str = "crash-job";
/// 進捗スレッドの照会間隔（ms）。
const POLL_MS: u64 = 3;
/// kill 待ちの sleep ループの上限回数（安全弁。1 回 100ms）。
const MAX_IDLE_LOOPS: u64 = 3000;
/// 進捗スレッドの照会回数の上限（安全弁）。
const MAX_POLLS: u64 = 2_000_000;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Delete,
    Update,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "delete" => Some(Mode::Delete),
            "update" => Some(Mode::Update),
            _ => None,
        }
    }

    fn job_sql(self) -> String {
        match self {
            Mode::Delete => format!(
                "DELETE FROM docs WHERE tag = 'x' USING OPERATION_ID '{JOB_OP}' PARTITIONED CHUNK {CHUNK}"
            ),
            Mode::Update => format!(
                "UPDATE docs SET tag = 'done' WHERE tag = 'x' USING OPERATION_ID '{JOB_OP}' PARTITIONED CHUNK {CHUNK}"
            ),
        }
    }

    /// 同じ operation_id・別述語の分割文（22023 になるはず）。
    fn mismatched_sql(self) -> String {
        match self {
            Mode::Delete => format!(
                "DELETE FROM docs WHERE tag = 'y' USING OPERATION_ID '{JOB_OP}' PARTITIONED CHUNK {CHUNK}"
            ),
            Mode::Update => format!(
                "UPDATE docs SET tag = 'done' WHERE tag = 'y' USING OPERATION_ID '{JOB_OP}' PARTITIONED CHUNK {CHUNK}"
            ),
        }
    }

    /// 同じ operation_id を使う通常（非 PARTITIONED）の DML（22023 になるはず）。
    fn plain_sql(self) -> String {
        match self {
            Mode::Delete => {
                format!("DELETE FROM docs WHERE tag = 'x' USING OPERATION_ID '{JOB_OP}'")
            }
            Mode::Update => format!(
                "UPDATE docs SET tag = 'done' WHERE tag = 'x' USING OPERATION_ID '{JOB_OP}'"
            ),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (sub, path, mode) = match (args.next(), args.next(), args.next()) {
        (Some(s), Some(p), Some(m)) => (s, p, m),
        _ => {
            eprintln!("usage: crash_tool_partitioned_dml <init|write|verify|finish> <db_path> <delete|update>");
            std::process::exit(2);
        }
    };
    let Some(mode) = Mode::parse(&mode) else {
        eprintln!("ERROR: unknown mode (expected delete|update)");
        std::process::exit(2);
    };
    let code = match sub.as_str() {
        "init" => report_err(init(&path)),
        "write" => report_err(write(&path, mode)),
        "verify" => match verify(&path, mode) {
            Ok((status, rows)) => {
                println!("RESULT ok=true status={status} rows={rows}");
                0
            }
            Err(reason) => {
                println!("RESULT ok=false reason={reason}");
                1
            }
        },
        "finish" => match finish(&path, mode) {
            Ok(rows) => {
                println!("RESULT ok=true status=completed rows={rows}");
                0
            }
            Err(reason) => {
                println!("RESULT ok=false reason={reason}");
                1
            }
        },
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(code);
}

fn report_err(r: Result<(), String>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

fn limits() -> Result<PartitionedDmlLimits, String> {
    let nz = |v: usize| std::num::NonZeroUsize::new(v).ok_or_else(|| "zero limit".to_string());
    Ok(PartitionedDmlLimits {
        scan_budget_rows: nz((ROWS_A as usize) * 4)?,
        max_writer_hold: Duration::from_millis(5000),
        ..PartitionedDmlLimits::default()
    })
}

fn open_core(path: &str) -> Result<EngineCore, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_partitioned_dml_limits(limits()?),
    )
}

fn ctx(tenant: &str) -> Result<PolicyContext, String> {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn sql(core: &EngineCore, tenant: &str, stmt: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant)?, &mut session, stmt)
        .map_err(|e| format!("{}:{e}", e.wire_code()))
}

/// SQL 失敗の wire_code だけを取り出す（`<code>:<message>` 形式の先頭）。
fn code_of(e: &str) -> &str {
    e.split(':').next().unwrap_or("")
}

fn seed_tenant(core: &EngineCore, tenant: &str, rows: u64) -> Result<(), String> {
    let mut start = 0;
    let mut n = 0;
    while start < rows {
        let end = (start + SEED_BATCH).min(rows);
        let values: Vec<String> = (start..end)
            .map(|id| {
                format!(
                    "({id}, '{}', {id})",
                    if id.is_multiple_of(2) { 'x' } else { 'y' }
                )
            })
            .collect();
        sql(
            core,
            tenant,
            &format!(
                "INSERT INTO docs (id, tag, n) VALUES {} USING OPERATION_ID 'seed-{n}'",
                values.join(", ")
            ),
        )?;
        start = end;
        n += 1;
    }
    Ok(())
}

fn init(path: &str) -> Result<(), String> {
    let core = open_core(path)?;
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("crash-pdml-sys")?,
        &mut session,
        "CREATE TABLE docs (tag TEXT, n BIGINT)",
    )
    .map_err(|e| format!("create table failed: {e}"))?;
    seed_tenant(&core, TENANT_A, ROWS_A)?;
    seed_tenant(&core, TENANT_B, ROWS_B)?;
    println!("SEEDED rows={}", ROWS_A + ROWS_B);
    Ok(())
}

/// `SHOW PARTITIONED DML` の結果（空なら `None`）。
fn show(core: &EngineCore, tenant: &str) -> Result<Option<(String, i64)>, String> {
    match sql(
        core,
        tenant,
        &format!("SHOW PARTITIONED DML '{JOB_OP}' ON docs"),
    )? {
        SqlOutcome::Query(q) => match q.rows.first() {
            None => Ok(None),
            Some(r) => match (r.cells.first(), r.cells.get(1)) {
                (Some(Cell::Text(s)), Some(Cell::SignedInteger(n))) => Ok(Some((s.clone(), *n))),
                other => Err(format!("unexpected SHOW cells: {other:?}")),
            },
        },
        other => Err(format!("unexpected SHOW outcome: {other:?}")),
    }
}

fn write(path: &str, mode: Mode) -> Result<(), String> {
    let core = Arc::new(open_core(path)?);
    let stop = Arc::new(AtomicBool::new(false));

    // 読み取り専用の進捗スレッド。値が変わったときだけ flush して出力する
    // （スクリプトの開始同期点と acked 行数の根拠）。
    let poller = {
        let core = Arc::clone(&core);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut last: Option<(String, i64)> = None;
            for _ in 0..MAX_POLLS {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(Some(cur)) = show(&core, TENANT_A) {
                    if last.as_ref() != Some(&cur) {
                        let out = std::io::stdout();
                        let mut h = out.lock();
                        let _ = writeln!(h, "PROGRESS status={} rows={}", cur.0, cur.1);
                        let _ = h.flush();
                        last = Some(cur);
                    }
                }
                std::thread::sleep(Duration::from_millis(POLL_MS));
            }
        })
    };

    let result = sql(&core, TENANT_A, &mode.job_sql());
    let marker = match result {
        Ok(SqlOutcome::Delete(o)) => format!("DONE rows={}", o.rows_affected),
        Ok(SqlOutcome::Update(o)) => format!("DONE rows={}", o.rows_affected),
        Ok(other) => return Err(format!("unexpected outcome: {other:?}")),
        Err(e) if code_of(&e) == "23505" => "ALREADY_COMPLETED".to_string(),
        Err(e) => return Err(format!("partitioned dml failed: {e}")),
    };
    // 進捗スレッドを止めてから最終行を出す（`DONE` が最後の完全な行になる）。
    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();
    {
        let out = std::io::stdout();
        let mut h = out.lock();
        writeln!(h, "{marker}").map_err(|e| format!("stdout write failed: {e}"))?;
        h.flush().map_err(|e| format!("stdout flush failed: {e}"))?;
    }
    for _ in 0..MAX_IDLE_LOOPS {
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("timed out waiting for kill".to_string())
}

/// `(id -> (tag, n))`。LIMIT 到達は fail-closed で拒否する。
fn read_rows(core: &EngineCore, tenant: &str) -> Result<BTreeMap<u64, (String, i64)>, String> {
    let q = match sql(
        core,
        tenant,
        &format!("SELECT id, tag, n FROM docs LIMIT {SCAN_LIMIT}"),
    )? {
        SqlOutcome::Query(q) => q,
        other => return Err(format!("unexpected SELECT outcome: {other:?}")),
    };
    if q.rows.len() >= SCAN_LIMIT {
        return Err(format!(
            "row count reached this tool's scan limit ({SCAN_LIMIT}); reduce ROWS"
        ));
    }
    let mut out = BTreeMap::new();
    for r in &q.rows {
        let (tag, n) = tag_n(&r.cells)?;
        out.insert(r.id, (tag, n));
    }
    Ok(out)
}

fn tag_n(cells: &[Cell]) -> Result<(String, i64), String> {
    let tag = cells.iter().find_map(|c| match c {
        Cell::Text(s) => Some(s.clone()),
        _ => None,
    });
    let n = cells.iter().find_map(|c| match c {
        Cell::SignedInteger(n) => Some(*n),
        _ => None,
    });
    match (tag, n) {
        (Some(t), Some(n)) => Ok((t, n)),
        _ => Err(format!("unexpected row cells: {cells:?}")),
    }
}

fn initial_tag(id: u64) -> &'static str {
    if id.is_multiple_of(2) {
        "x"
    } else {
        "y"
    }
}

fn recorded(core: &EngineCore, tenant: &str) -> Result<LedgerLookup, String> {
    let op = OperationId::parse(JOB_OP).map_err(|e| format!("op id: {e:?}"))?;
    core.operation_recorded(&ctx(tenant)?, "docs", &op)
        .map_err(|e| format!("operation_recorded failed: {e}"))
}

/// テナント B の行がすべて初期値で、ジョブ記録・台帳が無いことを確認する。
fn check_tenant_b(core: &EngineCore) -> Result<(), String> {
    let rows = read_rows(core, TENANT_B)?;
    if rows.len() as u64 != ROWS_B {
        return Err(format!("tenant B row count changed: {}", rows.len()));
    }
    for (id, (tag, n)) in &rows {
        if tag != initial_tag(*id) || *n != *id as i64 {
            return Err(format!("tenant B row {id} was modified"));
        }
    }
    if show(core, TENANT_B)?.is_some() {
        return Err("tenant B sees a job record".to_string());
    }
    if recorded(core, TENANT_B)? != LedgerLookup::NotRecorded {
        return Err("tenant B has a ledger entry".to_string());
    }
    Ok(())
}

fn verify(path: &str, mode: Mode) -> Result<(String, i64), String> {
    let core = open_core(path)?;
    let shown = show(&core, TENANT_A)?;
    let (status, rows_shown) = match &shown {
        None => ("none".to_string(), 0),
        Some((s, n)) => (s.clone(), *n),
    };
    // 再起動後は登録簿が空なので running は有り得ない（ADR 9.2 C1）。
    if !matches!(status.as_str(), "none" | "interrupted" | "completed") {
        return Err(format!("unexpected status after restart: {status}"));
    }
    let rows = read_rows(&core, TENANT_A)?;
    if rows.is_empty() {
        return Err("no rows found".to_string());
    }
    let matching: Vec<u64> = (0..ROWS_A).filter(|id| id % 2 == 0).collect();

    // 非一致行は存在し、内容が変わっていない。
    for id in (0..ROWS_A).filter(|id| id % 2 == 1) {
        match rows.get(&id) {
            Some((tag, n)) if tag == "y" && *n == id as i64 => {}
            other => return Err(format!("non-matching row {id} changed: {other:?}")),
        }
    }
    // 変更集合 D。
    let changed: BTreeSet<u64> = match mode {
        Mode::Delete => matching
            .iter()
            .copied()
            .filter(|id| !rows.contains_key(id))
            .collect(),
        Mode::Update => matching
            .iter()
            .copied()
            .filter(|id| rows.get(id).map(|(t, _)| t.as_str()) == Some("done"))
            .collect(),
    };
    // 一致行は「未変更」か「変更済み」のどちらかで、内容も整合している。
    for id in &matching {
        if changed.contains(id) {
            // UPDATE 済み行は tag だけが変わり n は不変（n == id）であること。
            // 更新済み行の n 破損を見逃さない（DELETE の変更済み行は存在しない）。
            if mode == Mode::Update {
                match rows.get(id) {
                    Some((tag, n)) if tag == "done" && *n == *id as i64 => {}
                    other => {
                        return Err(format!("updated row {id} is corrupted: {other:?}"));
                    }
                }
            }
            continue;
        }
        match rows.get(id) {
            Some((tag, n)) if tag == "x" && *n == *id as i64 => {}
            other => {
                return Err(format!(
                    "unchanged matching row {id} is corrupted: {other:?}"
                ))
            }
        }
    }
    // 前方一致: D は一致列の先頭部分列。
    let d = changed.len();
    let prefix: BTreeSet<u64> = matching.iter().copied().take(d).collect();
    if changed != prefix {
        return Err(
            "changed set is not a prefix of the matching ids (forward-match broken)".to_string(),
        );
    }
    // チャンク原子性（主）: |D| == SHOW の処理済み件数。
    if d as i64 != rows_shown {
        return Err(format!(
            "chunk atomicity broken: changed={d} but recorded progress={rows_shown}"
        ));
    }
    // チャンク原子性（補助）。
    if status == "interrupted" && !(d as u64).is_multiple_of(CHUNK) {
        return Err(format!("interrupted with partial chunk: changed={d}"));
    }
    // 台帳エントリ: 完了前 0 件・完了後 1 件。
    let led = recorded(&core, TENANT_A)?;
    match status.as_str() {
        "completed" => {
            if led != LedgerLookup::Recorded {
                return Err("completed job has no ledger entry".to_string());
            }
            if d != matching.len() {
                return Err(format!("completed but changed={d} != {}", matching.len()));
            }
        }
        _ => {
            if led != LedgerLookup::NotRecorded {
                return Err("ledger entry exists before completion".to_string());
            }
        }
    }
    check_tenant_b(&core)?;
    Ok((status, rows_shown))
}

fn finish(path: &str, mode: Mode) -> Result<i64, String> {
    let core = open_core(path)?;
    let before = show(&core, TENANT_A)?;
    let matching_total = (0..ROWS_A).filter(|id| id % 2 == 0).count() as i64;
    let returned = match sql(&core, TENANT_A, &mode.job_sql()) {
        Ok(SqlOutcome::Delete(o)) => o.rows_affected as i64,
        Ok(SqlOutcome::Update(o)) => o.rows_affected as i64,
        Ok(other) => return Err(format!("unexpected outcome: {other:?}")),
        // 既に完了済みなら件数は SHOW から取る。
        Err(e) if code_of(&e) == "23505" && matches!(&before, Some((s, _)) if s == "completed") => {
            before.as_ref().map(|b| b.1).unwrap_or(-1)
        }
        Err(e) => return Err(format!("resend failed: {e}")),
    };
    if returned != matching_total {
        return Err(format!(
            "exactly-once broken: returned={returned} expected={matching_total}"
        ));
    }
    if show(&core, TENANT_A)? != Some(("completed".to_string(), matching_total)) {
        return Err("SHOW is not completed with the full count".to_string());
    }
    let rows = read_rows(&core, TENANT_A)?;
    for id in 0..ROWS_A {
        let matching = id % 2 == 0;
        match (mode, matching, rows.get(&id)) {
            (Mode::Delete, true, None) => {}
            (Mode::Delete, true, Some(_)) => return Err(format!("row {id} was not deleted")),
            (Mode::Update, true, Some((t, n))) if t == "done" && *n == id as i64 => {}
            (Mode::Update, true, other) => return Err(format!("row {id} not updated: {other:?}")),
            (_, false, Some((t, n))) if t == "y" && *n == id as i64 => {}
            (_, false, other) => return Err(format!("non-matching row {id} changed: {other:?}")),
        }
    }
    if recorded(&core, TENANT_A)? != LedgerLookup::Recorded {
        return Err("ledger entry missing after completion".to_string());
    }
    match core
        .last_operation_id(&ctx(TENANT_A)?, "docs")
        .map_err(|e| format!("last_operation_id failed: {e}"))?
    {
        LastOperationLookup::Committed(op) if op.as_str() == JOB_OP => {}
        other => return Err(format!("unexpected last operation: {other:?}")),
    }
    // 再送・不一致・通常 DML との衝突の拒否コード。
    let expect = |stmt: String, want: &str| -> Result<(), String> {
        match sql(&core, TENANT_A, &stmt) {
            Err(e) if code_of(&e) == want => Ok(()),
            other => Err(format!("expected {want} for resend, got {other:?}")),
        }
    };
    expect(mode.job_sql(), "23505")?;
    expect(mode.mismatched_sql(), "22023")?;
    expect(mode.plain_sql(), "22023")?;
    check_tenant_b(&core)?;
    Ok(returned)
}
