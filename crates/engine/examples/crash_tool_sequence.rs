//! Issue #1692（TABLE-24 (3)(4)・RECOVER-10・RECOVER-11・RECOVER-12・PERSIST-1）の採番状態・
//! `operation_id` 台帳のクラッシュ耐性回帰テスト用ツール。`crash_tool_unique_index.rs` と役割・
//! 使い方の形は同じで、`scripts/crash_test_sequence.sh` から `write` をバックグラウンド起動して
//! SIGKILL し、続けて `verify` で再オープン後の整合を検証する。
//!
//! 本ツールは公開 API の `EngineCore::execute_sql_in_session`／`execute_sql_in_txn` だけを使う
//! （production の SQL 表層経路。採番は `nextval`、台帳は `USING OPERATION_ID`）。
//!
//! サブコマンド:
//! - `write <db_path>`: 表 `seq_rows (n BIGINT DEFAULT nextval('crash_seq'), via TEXT)` へ、1 行ずつ
//!   `RETURNING n` 付きで書き続ける。行 id から書き込みの形を決定的に選ぶ（`id % 3`）: 0 = 省略列の
//!   `DEFAULT nextval`、1 = VALUES の `nextval`、2 = `BEGIN` … `COMMIT` の明示トランザクション。
//!   どの形も `USING OPERATION_ID 'seq-<id>'` を付ける。成功のたびに
//!   `COMMITTED op=seq-<id> rows=<total> n=<value>` を出力して flush する（スクリプトの開始同期点と
//!   「kill 前に確認済みの commit」の根拠）。数行ごとに `BEGIN` → `INSERT … nextval` → `ROLLBACK` を
//!   挟み、メモリ上の値が永続値より先行した状態を作る（`ROLLEDBACK value=<v>` はログ専用）。
//! - `verify <db_path>`: 読み取り専用。オラクルは (1) 行 id が 1..=rows で連続する、(2) 全行の `n` が
//!   NULL でなく 1 以上、(3) **`n` が行 id の順に狭義単調増加**。(3) は、全行が `nextval` 由来で
//!   明示値を書かないことと、commit 済みの値が同じ txn の採番状態で覆われること（TABLE-24 (4)）から
//!   成り立ち、二重採番と、再起動後に commit 済みの値以下を払い出すことの両方を検出する。
//!   ROLLBACK した値の再出現はオラクルにしない（rolled back の払い出しは永続化の約束外。設計記録
//!   `docs/design/sequence-namespace-and-ledger.md` §11）。
//! - `finish <db_path>`: セットの最後に 1 回。副作用あり。(i) 最後に commit した id の文を同じ
//!   テキストで再送して `DuplicateOperationId`（`23505`）になり、行数・最大値が不変であること、
//!   (ii) 同じ op で SEQUENCE 名を変えた文が `OperationIdContentMismatch`（`22023`）になること、
//!   (iii) 新しい id で 1 行書くと値がちょうど `max + 1`（(i)(ii) が値を消費していない）であること
//!   を確かめ、最後に `verify` と同じオラクルを再適用する。
//! - `self-check <db_path>`: 負の対照。オラクルが空振りしないことを、不正な列（重複・逆順・欠落・
//!   NULL）と、DB へ明示値で書いた重複 `n` で確かめる。空の DB パスを渡す。

use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

/// write/verify で固定して使うテナント識別子（単一テナントのクラッシュ耐性検証が目的。
/// `crash_tool_unique_index.rs` と同方針）。
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// この crash ツールは行 id を行の識別に使う従来方式の契約を検証するため、`CREATE TABLE` を従来方式の
// 表作成へ振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;

const CRASH_TOOL_TENANT_ID: &str = "crash-tool-sequence-tenant";
/// 検証対象の表。
const TABLE: &str = "seq_rows";
/// 検証対象の SEQUENCE（昇順・NO CYCLE の既定）。
const SEQ: &str = "crash_seq";
/// 内容不一致プローブ用の SEQUENCE。
const SEQ2: &str = "crash_seq2";
/// `SELECT … LIMIT` の上限。SQL 表層が SELECT の `LIMIT` に課す上限（1..=10000）と同じ。
/// 達したら全件を見られないので fail-closed に拒否する。
const SCAN_LIMIT: u64 = 10_000;
/// write の自走上限（安全弁。通常は SIGKILL で止まるため到達しない）。
const MAX_ROWS: u64 = 500_000;
/// ROLLBACK 値を挟む間隔（行数）。
const ROLLBACK_EVERY: u64 = 4;

fn main() {
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let path = args.next();
    let (sub, path) = match (sub, path) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!("usage: crash_tool_sequence <write|verify|finish|self-check> <db_path>");
            std::process::exit(2);
        }
    };
    let exit_code = match sub.as_str() {
        "write" => run_write(&path),
        "verify" => report(verify_inner(&path)),
        "finish" => report(finish_inner(&path)),
        "self-check" => run_self_check(&path),
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(exit_code);
}

/// `RESULT ok=…` 行を出力して終了コードを返す。
fn report(result: Result<(u64, i64), String>) -> i32 {
    match result {
        Ok((rows, max)) => {
            println!("RESULT ok=true rows={rows} max={max}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

fn open_core(path: &str) -> Result<EngineCore, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

fn tenant_ctx() -> Result<PolicyContext, String> {
    // SQL 表層の INSERT は可視性を `Private` 固定で書くため、両方を許可する
    // （`crash_tool_unique_index.rs` と同じ理由）。
    PolicyContext::with_visibilities(
        CRASH_TOOL_TENANT_ID,
        [Visibility::Public, Visibility::Private],
    )
    .map_err(|e| format!("invalid tenant context: {e}"))
}

/// 既に存在する場合のエラー（`42710`・`42P07`）かどうか。
fn is_already_exists(e: &SqlSurfaceError) -> bool {
    matches!(e.wire_code(), "42710" | "42P07")
}

/// SEQUENCE・表を冪等に作る（既存 DB の再オープン時は重複エラーを無視する）。
fn ensure_schema(core: &EngineCore, ctx: &PolicyContext) -> Result<(), String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        format!("CREATE SEQUENCE {SEQ}"),
        format!("CREATE SEQUENCE {SEQ2}"),
        format!("CREATE TABLE {TABLE} (n BIGINT DEFAULT nextval('{SEQ}'), via TEXT)"),
    ] {
        match core.execute_sql_in_session_legacy(ctx, &mut session, &sql) {
            Ok(_) => {}
            Err(e) if is_already_exists(&e) => {}
            Err(e) => return Err(format!("ddl failed ({sql}): {e}")),
        }
    }
    Ok(())
}

/// 行 id から決まる書き込みの形。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// 省略列の `DEFAULT nextval`。
    Default,
    /// VALUES の `nextval`。
    Values,
    /// `BEGIN` … `COMMIT` の明示トランザクション内の VALUES `nextval`。
    Txn,
}

fn shape_of(id: u64) -> Shape {
    match id % 3 {
        0 => Shape::Default,
        1 => Shape::Values,
        _ => Shape::Txn,
    }
}

/// 行 id の INSERT 文（形ごと。`RETURNING n` と `USING OPERATION_ID` 付き）。再送は同じテキストを
/// 組み立て直すことで再現する。
fn insert_sql(id: u64) -> String {
    match shape_of(id) {
        Shape::Default => format!(
            "INSERT INTO {TABLE} (id, via) VALUES ({id}, 'default') RETURNING n \
             USING OPERATION_ID 'seq-{id}'"
        ),
        // 明示トランザクション形も文面は VALUES 形と同一（BEGIN／COMMIT で包むだけ）。再送時に
        // autocommit で送っても内容照合ハッシュは同じになる。
        Shape::Values | Shape::Txn => format!(
            "INSERT INTO {TABLE} (id, n, via) VALUES ({id}, nextval('{SEQ}'), 'values') \
             RETURNING n USING OPERATION_ID 'seq-{id}'"
        ),
    }
}

/// `RETURNING n` の結果から払い出された値を取り出す。
fn returned_value(outcome: SqlOutcome) -> Result<i64, String> {
    match outcome {
        SqlOutcome::Returning(o) => match o.result.rows.first().and_then(|r| r.cells.first()) {
            Some(Cell::SignedInteger(v)) => Ok(*v),
            other => Err(format!("unexpected RETURNING cell: {other:?}")),
        },
        other => Err(format!("expected RETURNING outcome, got {other:?}")),
    }
}

/// 行 id の INSERT を形どおりに実行し、払い出された値を返す。
fn execute_row(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Result<i64, String> {
    let sql = insert_sql(id);
    let mut session = SessionState::default();
    if shape_of(id) == Shape::Txn {
        let mut txn = core.new_session_transaction();
        let mut value = None;
        for stmt in ["BEGIN", sql.as_str(), "COMMIT"] {
            let out = core
                .execute_sql_in_txn(ctx, &mut session, &mut txn, stmt)
                .map_err(|e| format!("txn statement failed at id={id} ({stmt}): {e}"))?;
            if stmt == sql {
                value = Some(returned_value(out)?);
            }
        }
        value.ok_or_else(|| "txn insert produced no value".to_string())
    } else {
        let out = core
            .execute_sql_in_session_legacy(ctx, &mut session, &sql)
            .map_err(|e| format!("insert failed at id={id}: {e}"))?;
        returned_value(out)
    }
}

/// `BEGIN` → `INSERT … nextval` → `ROLLBACK`。メモリ上の値を永続値より先行させる（ログ専用）。
fn rolled_back_probe(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Result<i64, String> {
    let sql = format!(
        "INSERT INTO {TABLE} (id, n, via) VALUES ({id}, nextval('{SEQ}'), 'rolledback') \
         RETURNING n USING OPERATION_ID 'rb-{id}'"
    );
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut value = None;
    for stmt in ["BEGIN", sql.as_str(), "ROLLBACK"] {
        let out = core
            .execute_sql_in_txn(ctx, &mut session, &mut txn, stmt)
            .map_err(|e| format!("rollback probe failed at id={id} ({stmt}): {e}"))?;
        if stmt == sql {
            value = Some(returned_value(out)?);
        }
    }
    value.ok_or_else(|| "rollback probe produced no value".to_string())
}

/// 1 行分の `(行 id, n)`。`n` が NULL の行は `None`。
type RowPair = (u64, Option<i64>);

/// 全行を `(id, n)` の id 昇順で読む。`n` が NULL の行は `None`。上限に達したら fail-closed。
fn read_rows(core: &EngineCore, ctx: &PolicyContext) -> Result<Vec<RowPair>, String> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id, n FROM {TABLE} LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("scan failed: {e}"))?;
    if result.rows.len() as u64 >= SCAN_LIMIT {
        return Err(format!(
            "row count reached this tool's scan limit ({SCAN_LIMIT}); reduce the crash \
             test's sets/iterations"
        ));
    }
    let mut rows = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let id = row.id;
        let n = match row.cells.get(1) {
            Some(Cell::SignedInteger(v)) => Some(*v),
            Some(Cell::Null) => None,
            other => return Err(format!("unexpected n cell for id={id}: {other:?}")),
        };
        rows.push((id, n));
    }
    rows.sort_unstable();
    Ok(rows)
}

/// オラクル本体（純関数。`self-check` が不正な列で `Err` になることを確かめる）。
/// 戻り値は `(行数, 最大の n)`。
fn check_rows(rows: &[RowPair]) -> Result<(u64, i64), String> {
    if rows.is_empty() {
        return Err("no rows found".to_string());
    }
    let mut prev: Option<i64> = None;
    for (index, (id, n)) in rows.iter().enumerate() {
        let expected = index as u64 + 1;
        if *id != expected {
            return Err(format!(
                "id gap or disorder: expected={expected} actual={id}"
            ));
        }
        let Some(n) = n else {
            return Err(format!("n is NULL at id={id}"));
        };
        if *n < 1 {
            return Err(format!("n={n} is below the sequence start at id={id}"));
        }
        if let Some(p) = prev {
            if *n <= p {
                return Err(format!(
                    "n is not strictly increasing: id={id} n={n} previous={p} \
                     (double allocation or re-issued committed value)"
                ));
            }
        }
        prev = Some(*n);
    }
    Ok((rows.len() as u64, prev.unwrap_or(0)))
}

fn verify_with(core: &EngineCore, ctx: &PolicyContext) -> Result<(u64, i64), String> {
    check_rows(&read_rows(core, ctx)?)
}

fn verify_inner(path: &str) -> Result<(u64, i64), String> {
    let core = open_core(path)?;
    let ctx = tenant_ctx()?;
    verify_with(&core, &ctx)
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
    let ctx = tenant_ctx()?;
    ensure_schema(&core, &ctx)?;
    // 再開: 既存行の最大 id の次から続ける（verify が行 id の連続性を保証している）。
    let existing = read_rows(&core, &ctx)?;
    let mut next_id = existing
        .last()
        .map_or(Ok(1), |(id, _)| id.checked_add(1).ok_or("row id overflow"))?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for _ in 0..MAX_ROWS {
        if next_id % ROLLBACK_EVERY == 0 {
            let v = rolled_back_probe(&core, &ctx, next_id)?;
            writeln!(handle, "ROLLEDBACK value={v}")
                .map_err(|e| format!("stdout write failed: {e}"))?;
        }
        let value = execute_row(&core, &ctx, next_id)?;
        // 進捗行はスクリプトの同期点なので毎回 flush する。行 id は 1 起点で連続するため
        // `rows` は id と一致する。
        writeln!(
            handle,
            "COMMITTED op=seq-{next_id} rows={next_id} n={value}"
        )
        .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
        next_id = next_id.checked_add(1).ok_or("row id overflow")?;
        // 意図的なスロットリング: 累積行数が `SCAN_LIMIT` に達する前に SIGKILL されるようにする
        // （`crash_tool_unique_index.rs` と同方針）。
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    Ok(())
}

/// 最後の commit を再送し、内容不一致を拒否させ、新しい 1 行を書いて値の消費の有無を確かめる。
fn finish_inner(path: &str) -> Result<(u64, i64), String> {
    let core = open_core(path)?;
    let ctx = tenant_ctx()?;
    let (rows, max) = verify_with(&core, &ctx)?;
    let last_id = rows;

    // (i) 同じ op・同じ内容の再送は台帳が拒否する（再実行も採番もしない）。
    let mut session = SessionState::default();
    match core.execute_sql_in_session_legacy(&ctx, &mut session, &insert_sql(last_id)) {
        Err(SqlSurfaceError::DuplicateOperationId) => {}
        Err(e) => {
            return Err(format!(
                "resend failed with an unexpected error (expected DuplicateOperationId): {e}"
            ))
        }
        Ok(_) => return Err("resend of a committed operation_id was wrongly accepted".to_string()),
    }
    let after = verify_with(&core, &ctx)?;
    if after != (rows, max) {
        return Err(format!(
            "resend changed the table: before=({rows},{max}) after=({},{})",
            after.0, after.1
        ));
    }

    // (ii) 同じ op で SEQUENCE 名を変えた文は内容不一致（採番前に拒否される）。
    let mismatch = format!(
        "INSERT INTO {TABLE} (id, n, via) VALUES ({last_id}, nextval('{SEQ2}'), 'values') \
         RETURNING n USING OPERATION_ID 'seq-{last_id}'"
    );
    match core.execute_sql_in_session_legacy(&ctx, &mut session, &mismatch) {
        Err(SqlSurfaceError::OperationIdContentMismatch) => {}
        Err(e) => {
            return Err(format!(
                "mismatch probe failed with an unexpected error (expected \
                 OperationIdContentMismatch): {e}"
            ))
        }
        Ok(_) => return Err("mismatch probe was wrongly accepted".to_string()),
    }

    // (ii-b) 内容不一致の文が参照した SEQUENCE も消費されていない（SEQ2 は finish でしか使わず、
    // 内容照合前に払い出していれば初期値 1 を飛ばす）。新しい op で払い出して 1 を確認する。
    let probe2_id = last_id.checked_add(1).ok_or("row id overflow")?;
    let probe2_sql = format!(
        "INSERT INTO {TABLE} (id, n, via) VALUES ({probe2_id}, nextval('{SEQ2}'), 'probe2') \
         RETURNING n USING OPERATION_ID 'seq-probe2-{probe2_id}'"
    );
    let mut txn = core.new_session_transaction();
    let mut v2 = None;
    for stmt in ["BEGIN", probe2_sql.as_str(), "ROLLBACK"] {
        let out = core
            .execute_sql_in_txn(&ctx, &mut session, &mut txn, stmt)
            .map_err(|e| format!("seq2 probe failed ({stmt}): {e}"))?;
        if stmt == probe2_sql {
            v2 = Some(returned_value(out)?);
        }
    }
    if v2 != Some(1) {
        return Err(format!(
            "{SEQ2} first value is {v2:?}, expected 1 (a mismatch-rejected statement consumed a value)"
        ));
    }

    // (iii) 新しい id で VALUES 形を書く。(i)(ii) が値を消費していなければ、値はちょうど max + 1
    // （再起動直後のプロセスなので、メモリ上の先行払い出しは無い）。
    let new_id = last_id.checked_add(1).ok_or("row id overflow")?;
    let out = core
        .execute_sql_in_session_legacy(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, n, via) VALUES ({new_id}, nextval('{SEQ}'), 'finish') \
                 RETURNING n USING OPERATION_ID 'seq-finish-{new_id}'"
            ),
        )
        .map_err(|e| format!("finish insert failed: {e}"))?;
    let value = returned_value(out)?;
    let expected = max.checked_add(1).ok_or("value overflow")?;
    if value != expected {
        return Err(format!(
            "next value after resend/mismatch probes is {value}, expected {expected} \
             (a rejected statement consumed a value, or a committed value was re-issued)"
        ));
    }
    verify_with(&core, &ctx)
}

/// 負の対照。オラクルが不正な状態を検出できる（`ok=false` を返す）ことを確かめる。
fn run_self_check(path: &str) -> i32 {
    match self_check_inner(path) {
        Ok(()) => {
            println!("SELF-CHECK ok=true");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK ok=false reason={reason}");
            1
        }
    }
}

fn self_check_inner(path: &str) -> Result<(), String> {
    // 純関数: 不正な列はすべて拒否され、正しい列は受理される。
    let bad: [(&str, Vec<RowPair>); 6] = [
        ("empty", vec![]),
        ("duplicate n", vec![(1, Some(3)), (2, Some(3))]),
        ("reversed n", vec![(1, Some(5)), (2, Some(2))]),
        ("id gap", vec![(1, Some(1)), (3, Some(2))]),
        ("null n", vec![(1, Some(1)), (2, None)]),
        ("zero n", vec![(1, Some(0))]),
    ];
    for (label, rows) in &bad {
        if check_rows(rows).is_ok() {
            return Err(format!("oracle accepted a bad sequence: {label}"));
        }
    }
    // 欠番（n の飛び）は正当（ROLLBACK・失敗した文の欠番）。
    if check_rows(&[(1, Some(1)), (2, Some(7))]) != Ok((2, 7)) {
        return Err("oracle rejected a valid gapped sequence".to_string());
    }
    // DB 経由: 明示値で重複 n を書くと、verify 本体が検出する。
    let core = open_core(path)?;
    let ctx = tenant_ctx()?;
    ensure_schema(&core, &ctx)?;
    let mut session = SessionState::default();
    for (id, n) in [(1u64, 5i64), (2, 5)] {
        core.execute_sql_in_session_legacy(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, n, via) VALUES ({id}, {n}, 'selfcheck') \
                 USING OPERATION_ID 'selfcheck-{id}'"
            ),
        )
        .map_err(|e| format!("self-check insert failed: {e}"))?;
    }
    if verify_with(&core, &ctx).is_ok() {
        return Err("oracle accepted a duplicated n written to the database".to_string());
    }
    Ok(())
}
