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
//!
//! Issue #1700（TABLE-24・RECOVER-10〜12）の migrated-id モード（`*-identity` サブコマンド）:
//! 従来方式の表 `mig_rows (n BIGINT, via TEXT)` を作って開き直し（移行で所有 SEQUENCE
//! `mig_rows_id_seq` が付く）、`id` を省略した INSERT の採番を検証する。
//! - `write-identity <db_path>`: 連番 `n`（書き込み順）の行を、文の形（単一行・2 行・3 行・明示
//!   トランザクション）を文の通し番号 `% 4` で決めて書き続ける。どの文も `RETURNING id` と
//!   `USING OPERATION_ID 'mig-<first_n>'` を付け、成功ごとに
//!   `COMMITTED op=mig-<first_n> rows=<総行数> n=<最後の id>` を flush する。数行ごとに
//!   `BEGIN` → `id` 省略 INSERT → `ROLLBACK` を挟む。
//! - `verify-identity <db_path>`: 読み取り専用。オラクルは (1) `id` が 1 以上で一意、(2) `n` が
//!   1..=行数で連続（文は全か無か）、(3) 行数が文の境界、(4) **`n` の順に `id` が狭義単調増加**
//!   （二重採番と、再起動後に commit 済みの値以下の払い出しを検出する）。
//! - `finish-identity <db_path>`: 最後に commit した文を同じテキストで再送して `23505`、内容を変えた
//!   再送が `22023` になり、いずれも行数・最大 `id` を変えず、続く新しい op の 1 行がちょうど
//!   `max + 1` を採番する（再送が値を消費していない）ことを確かめる。
//! - `self-check-identity <db_path>`: 負の対照（不正な列と、DB へ逆順の `id` を明示して書いた表）。
//!
//! Issue #2229（SQL-68・RLS-10・RECOVER-11・RECOVER-12）の `DO` ブロックの段（`*-do` サブコマンド）:
//! 表 `do_a`（k ごとに 3 行）・`do_b`（k ごとに 1 行）・証人 `do_w` へ書く `DO` を、偶数 k は autocommit、
//! 奇数 k は明示 Tx の `DO` + `USING OPERATION_ID 'do-<k>'` 付き証人 INSERT で書き続ける。
//! - `write-do <db_path>`: 成功ごとに `COMMITTED op=do-<k> rows=<do_a の総行数> n=<適用済みの k の個数>`。
//! - `verify-do <db_path>`: 読み取り専用。オラクルは (1) `do_a` と `do_b` が k ごとに全か無か、(2) 適用済み
//!   の k が 0 からの連続、(3) 奇数 k で台帳・証人・本体が一致（偶数 k は台帳なし）、(4) 第 2 テナントの
//!   行・台帳が不変。`RESULT` の `rows` は `do_a` の行数、`max` は適用済みの k の個数。
//! - `finish-do <db_path>`: 最後の奇数 k を順序 B（`DO` → 証人 INSERT）で再送して `23505`・全体の破棄、
//!   `DO ... USING OPERATION_ID` が `0A000`、新しい k がちょうど 1 回分適用されることを確かめる。
//! - `self-check-do <db_path>`: 負の対照（オラクルの純関数と、DB へ書いた不完全な状態）。

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
            eprintln!(
                "usage: crash_tool_sequence <write|verify|finish|self-check|write-identity|\
                 verify-identity|finish-identity|self-check-identity|write-do|verify-do|finish-do|\
                 self-check-do> <db_path>"
            );
            std::process::exit(2);
        }
    };
    let exit_code = match sub.as_str() {
        "write" => run_write(&path),
        "verify" => report(verify_inner(&path)),
        "finish" => report(finish_inner(&path)),
        "self-check" => run_self_check(&path),
        "write-identity" => run_write_identity(&path),
        "verify-identity" => report(identity::verify_inner(&path)),
        "finish-identity" => report(identity::finish_inner(&path)),
        "self-check-identity" => identity::run_self_check(&path),
        "write-do" => run_write_do(&path),
        "verify-do" => report(do_block::verify_inner(&path)),
        "finish-do" => report(do_block::finish_inner(&path)),
        "self-check-do" => do_block::run_self_check(&path),
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
/// Issue #1700 の migrated-id モード（`id` を省略した INSERT の採番）。既存モードの表・SEQUENCE とは
/// 独立した表 `mig_rows` を使う（同じ DB パスで両モードを混ぜない。スクリプトはモードごとに新規 DB）。
mod identity {
    use super::*;

    /// 検証対象の表（従来方式で作り、開き直しの移行で所有 SEQUENCE を持つ）。
    const MIG_TABLE: &str = "mig_rows";
    /// 文の形のサイクル長（文の通し番号 `% 4`）。
    const SHAPES: u64 = 4;

    /// 開く（開く時点の移行を走らせるため `EngineCore::open` を使う）。
    fn open(path: &str) -> Result<EngineCore, String> {
        EngineCore::open(path).map_err(|e| format!("open failed: {e}"))
    }

    /// 表を従来方式で冪等に作り、いったん閉じて開き直す（移行 2 段目で所有 SEQUENCE が付く）。
    fn open_ready(path: &str) -> Result<EngineCore, String> {
        {
            let core = open(path)?;
            let ctx = tenant_ctx()?;
            let mut session = SessionState::default();
            session.allow_ddl();
            let sql = format!("CREATE TABLE {MIG_TABLE} (n BIGINT, via TEXT)");
            match core.execute_sql_in_session_legacy(&ctx, &mut session, &sql) {
                Ok(_) => {}
                Err(e) if is_already_exists(&e) => {}
                Err(e) => return Err(format!("ddl failed ({sql}): {e}")),
            }
        }
        open(path)
    }

    /// 文の通し番号（0 始まり）から、その文の行数と、明示トランザクションで包むか。
    /// 行番号 `first_n` は 1 文あたり 1〜3 ずつ進み剰余が偏るため、形は通し番号で選ぶ
    /// （全 4 形に到達する）。復旧後も `statements` が文境界から通し番号を再構成する。
    fn shape_of(stmt_idx: u64) -> (u64, bool) {
        match stmt_idx % SHAPES {
            0 => (2, false),
            1 => (3, false),
            2 => (1, true),
            _ => (1, false),
        }
    }

    /// `upto_rows` 行目までを覆う文の列 `(first_n, 行数, 明示 txn か)`（文は 1 から始まり、前の文の
    /// 次の `n` から続く）。
    fn statements(upto_rows: u64) -> Vec<(u64, u64, bool)> {
        let mut out = Vec::new();
        let mut first = 1u64;
        let mut idx = 0u64;
        while first <= upto_rows {
            let (len, txn) = shape_of(idx);
            out.push((first, len, txn));
            first += len;
            idx += 1;
        }
        out
    }

    /// 文の INSERT（`id` 省略・`RETURNING id`・`USING OPERATION_ID`）。再送は同じテキストの組み直し。
    fn insert_sql(first_n: u64, len: u64, via: &str) -> String {
        let values: Vec<String> = (0..len)
            .map(|i| format!("({}, '{via}')", first_n + i))
            .collect();
        format!(
            "INSERT INTO {MIG_TABLE} (n, via) VALUES {} RETURNING id \
             USING OPERATION_ID 'mig-{first_n}'",
            values.join(", ")
        )
    }

    /// `RETURNING id` の全行の `id`。
    fn returned_ids(outcome: SqlOutcome) -> Result<Vec<u64>, String> {
        match outcome {
            SqlOutcome::Returning(o) => o
                .result
                .rows
                .iter()
                .map(|r| match r.cells.first() {
                    Some(Cell::Integer(v)) => Ok(*v),
                    other => Err(format!("unexpected RETURNING cell: {other:?}")),
                })
                .collect(),
            other => Err(format!("expected RETURNING outcome, got {other:?}")),
        }
    }

    fn execute_statement(
        core: &EngineCore,
        ctx: &PolicyContext,
        first_n: u64,
        len: u64,
        txn_wrapped: bool,
    ) -> Result<Vec<u64>, String> {
        let sql = insert_sql(first_n, len, "ident");
        let mut session = SessionState::default();
        if txn_wrapped {
            let mut txn = core.new_session_transaction();
            let mut ids = None;
            for stmt in ["BEGIN", sql.as_str(), "COMMIT"] {
                let out = core
                    .execute_sql_in_txn(ctx, &mut session, &mut txn, stmt)
                    .map_err(|e| format!("txn statement failed at n={first_n} ({stmt}): {e}"))?;
                if stmt == sql {
                    ids = Some(returned_ids(out)?);
                }
            }
            ids.ok_or_else(|| "txn insert produced no ids".to_string())
        } else {
            let out = core
                .execute_sql_in_session_legacy(ctx, &mut session, &sql)
                .map_err(|e| format!("insert failed at n={first_n}: {e}"))?;
            returned_ids(out)
        }
    }

    /// `BEGIN` → `id` 省略 INSERT → `ROLLBACK`（メモリ上の採番状態を永続値より先行させる。ログ専用）。
    fn rolled_back_probe(
        core: &EngineCore,
        ctx: &PolicyContext,
        first_n: u64,
    ) -> Result<u64, String> {
        let sql = format!(
            "INSERT INTO {MIG_TABLE} (n, via) VALUES (0, 'rolledback') RETURNING id \
             USING OPERATION_ID 'mig-rb-{first_n}'"
        );
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        let mut id = None;
        for stmt in ["BEGIN", sql.as_str(), "ROLLBACK"] {
            let out = core
                .execute_sql_in_txn(ctx, &mut session, &mut txn, stmt)
                .map_err(|e| format!("rollback probe failed at n={first_n} ({stmt}): {e}"))?;
            if stmt == sql {
                id = returned_ids(out)?.first().copied();
            }
        }
        id.ok_or_else(|| "rollback probe produced no id".to_string())
    }

    /// 1 行分の `(id, n)`。
    type Pair = (u64, i64);

    fn read_rows(core: &EngineCore, ctx: &PolicyContext) -> Result<Vec<Pair>, String> {
        let result = core
            .execute_sql(
                ctx,
                &format!("SELECT id, n FROM {MIG_TABLE} LIMIT {SCAN_LIMIT}"),
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
            match row.cells.get(1) {
                Some(Cell::SignedInteger(n)) => rows.push((row.id, *n)),
                other => return Err(format!("unexpected n cell for id={}: {other:?}", row.id)),
            }
        }
        Ok(rows)
    }

    /// `n` の順に並べ、`n` が 1..=行数で連続し、`id` が 1 以上で狭義単調増加であることを確かめる
    /// （文の境界の検査は含まない）。戻り値は `n` 順の `(id, n)`。
    fn check_order(rows: &[Pair]) -> Result<Vec<Pair>, String> {
        if rows.is_empty() {
            return Err("no rows found".to_string());
        }
        let mut by_n: Vec<Pair> = rows.to_vec();
        by_n.sort_unstable_by_key(|(_, n)| *n);
        let mut prev_id: Option<u64> = None;
        for (index, (id, n)) in by_n.iter().enumerate() {
            let expected = index as i64 + 1;
            if *n != expected {
                return Err(format!(
                    "n gap or duplicate (a partial statement or lost row): \
                     expected={expected} actual={n}"
                ));
            }
            if *id < 1 {
                return Err(format!("id={id} is below the sequence start at n={n}"));
            }
            if let Some(p) = prev_id {
                if *id <= p {
                    return Err(format!(
                        "id is not strictly increasing in n order: n={n} id={id} previous={p} \
                         (double allocation or re-issued committed value)"
                    ));
                }
            }
            prev_id = Some(*id);
        }
        Ok(by_n)
    }

    /// オラクル本体（純関数）。戻り値は `(行数, 最大の id)`。
    pub(super) fn check_rows(rows: &[Pair]) -> Result<(u64, i64), String> {
        let by_n = check_order(rows)?;
        let total = by_n.len() as u64;
        if !statements(total)
            .last()
            .is_some_and(|(first, len, _)| first + len - 1 == total)
        {
            return Err(format!("row count {total} is not a statement boundary"));
        }
        let max = by_n.last().map_or(0, |(id, _)| *id);
        let max = i64::try_from(max).map_err(|_| "id overflow".to_string())?;
        Ok((total, max))
    }

    fn verify_with(core: &EngineCore, ctx: &PolicyContext) -> Result<(u64, i64), String> {
        check_rows(&read_rows(core, ctx)?)
    }

    pub(super) fn verify_inner(path: &str) -> Result<(u64, i64), String> {
        let core = open(path)?;
        let ctx = tenant_ctx()?;
        verify_with(&core, &ctx)
    }

    pub(super) fn write_inner(path: &str) -> Result<(), String> {
        let core = open_ready(path)?;
        let ctx = tenant_ctx()?;
        let existing = read_rows(&core, &ctx)?;
        // 再開: 既存の行数の次の `n` から続ける（verify が文の境界を保証している）。
        let first_idx = u64::try_from(statements(u64::try_from(existing.len()).unwrap_or(0)).len())
            .map_err(|_| "statement count overflow".to_string())?;
        let mut next_n = u64::try_from(existing.len())
            .map_err(|_| "row count overflow".to_string())?
            .checked_add(1)
            .ok_or("n overflow")?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        for stmt_idx in first_idx..first_idx.saturating_add(MAX_ROWS) {
            let (len, txn_wrapped) = shape_of(stmt_idx);
            if next_n % ROLLBACK_EVERY == 0 {
                let id = rolled_back_probe(&core, &ctx, next_n)?;
                writeln!(handle, "ROLLEDBACK id={id}")
                    .map_err(|e| format!("stdout write failed: {e}"))?;
            }
            let ids = execute_statement(&core, &ctx, next_n, len, txn_wrapped)?;
            let last_id = ids.last().copied().ok_or("statement returned no ids")?;
            let total = next_n + len - 1;
            writeln!(handle, "COMMITTED op=mig-{next_n} rows={total} n={last_id}")
                .map_err(|e| format!("stdout write failed: {e}"))?;
            handle
                .flush()
                .map_err(|e| format!("stdout flush failed: {e}"))?;
            next_n = total.checked_add(1).ok_or("n overflow")?;
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Ok(())
    }

    pub(super) fn finish_inner(path: &str) -> Result<(u64, i64), String> {
        let core = open(path)?;
        let ctx = tenant_ctx()?;
        let (rows, max) = verify_with(&core, &ctx)?;
        let (first, len, _) = *statements(rows).last().ok_or("no statements")?;

        // (i) 同じ op・同じ内容の再送は台帳が拒否する（再実行も採番もしない）。
        let mut session = SessionState::default();
        match core.execute_sql_in_session_legacy(
            &ctx,
            &mut session,
            &insert_sql(first, len, "ident"),
        ) {
            Err(SqlSurfaceError::DuplicateOperationId) => {}
            Err(e) => {
                return Err(format!(
                    "resend failed with an unexpected error (expected DuplicateOperationId): {e}"
                ))
            }
            Ok(_) => return Err("resend of a committed operation_id was wrongly accepted".into()),
        }
        let after = verify_with(&core, &ctx)?;
        if after != (rows, max) {
            return Err(format!(
                "resend changed the table: before=({rows},{max}) after=({},{})",
                after.0, after.1
            ));
        }
        // (ii) 内容を変えた再送は内容不一致（採番前に拒否される）。
        match core.execute_sql_in_session_legacy(
            &ctx,
            &mut session,
            &insert_sql(first, len, "changed"),
        ) {
            Err(SqlSurfaceError::OperationIdContentMismatch) => {}
            Err(e) => {
                return Err(format!(
                    "mismatch probe failed with an unexpected error (expected \
                     OperationIdContentMismatch): {e}"
                ))
            }
            Ok(_) => return Err("mismatch probe was wrongly accepted".to_string()),
        }
        // (iii) 新しい op の 1 行はちょうど max + 1（再起動直後のプロセスで、再送が値を消費していない）。
        let new_n = rows.checked_add(1).ok_or("n overflow")?;
        let out = core
            .execute_sql_in_session_legacy(
                &ctx,
                &mut session,
                &format!(
                    "INSERT INTO {MIG_TABLE} (n, via) VALUES ({new_n}, 'finish') RETURNING id \
                     USING OPERATION_ID 'mig-finish-{new_n}'"
                ),
            )
            .map_err(|e| format!("finish insert failed: {e}"))?;
        let ids = returned_ids(out)?;
        let expected = u64::try_from(max)
            .map_err(|_| "max overflow".to_string())?
            .checked_add(1)
            .ok_or("id overflow")?;
        if ids != vec![expected] {
            return Err(format!(
                "next id after resend/mismatch probes is {ids:?}, expected {expected} \
                 (a rejected statement consumed a value, or a committed value was re-issued)"
            ));
        }
        // 1 行追加した状態は文の境界とは限らないため、`n` と `id` の順序だけを再確認する。
        let by_n = check_order(&read_rows(&core, &ctx)?)?;
        let max_after = by_n.last().map_or(0, |(id, _)| *id);
        Ok((
            by_n.len() as u64,
            i64::try_from(max_after).map_err(|_| "id overflow".to_string())?,
        ))
    }

    pub(super) fn run_self_check(path: &str) -> i32 {
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
        let bad: [(&str, Vec<Pair>); 6] = [
            ("empty", vec![]),
            ("duplicate id", vec![(1, 1), (1, 2)]),
            ("reversed id", vec![(5, 1), (2, 2)]),
            ("n gap", vec![(1, 1), (2, 3)]),
            ("zero id", vec![(0, 1)]),
            // n=1 だけでは、2 行の文（first_n=1）の途中で終わっている。
            ("partial statement", vec![(1, 1)]),
        ];
        for (label, rows) in &bad {
            if check_rows(rows).is_ok() {
                return Err(format!("oracle accepted a bad sequence: {label}"));
            }
        }
        // 欠番（id の飛び）は正当（ROLLBACK・失敗した文の欠番）。n=1..=2 は 2 行の文の境界。
        if check_rows(&[(1, 1), (4, 2)]) != Ok((2, 4)) {
            return Err("oracle rejected a valid gapped sequence".to_string());
        }
        // DB 経由: 逆順の id を明示して書くと verify 本体が検出する。
        let core = open_ready(path)?;
        let ctx = tenant_ctx()?;
        let mut session = SessionState::default();
        core.execute_sql_in_session_legacy(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {MIG_TABLE} (id, n, via) \
                 VALUES (5, 1, 'selfcheck'), (2, 2, 'selfcheck') \
                 USING OPERATION_ID 'mig-selfcheck-1'"
            ),
        )
        .map_err(|e| format!("self-check insert failed: {e}"))?;
        if verify_with(&core, &ctx).is_ok() {
            return Err("oracle accepted reversed ids written to the database".to_string());
        }
        Ok(())
    }
}

fn run_write_identity(path: &str) -> i32 {
    match identity::write_inner(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}
/// Issue #2229（SQL-68・RLS-10・RECOVER-11・RECOVER-12）の `DO` ブロックの段（`*-do` サブコマンド）。
///
/// `DO` の本体（複数表への INSERT）が「全か無か」で永続化され、明示 Tx で `USING OPERATION_ID` 付きの
/// 証人 INSERT と同じコミットに載せた場合は台帳と本体の書き込みが原子的であることを、SIGKILL 後の
/// 再オープンで検証する。`DO` 自体は `USING OPERATION_ID` を受理しない（`0A000`）ため、台帳の鍵は
/// 証人表 `do_w` への INSERT が担う（設計記録 `docs/design/plpgsql-subset.md` 「Issue #2229 追記」）。
mod do_block {
    use super::*;

    /// 本体が 3 行を書く表（`k` = 反復番号、`j` = 1..=3）。
    const TABLE_A: &str = "do_a";
    /// 本体が 1 行を書く表（`k` ごとにちょうど 1 行）。
    const TABLE_B: &str = "do_b";
    /// 証人表（奇数 `k` の明示 Tx で `USING OPERATION_ID 'do-<k>'` と一緒に書く）。
    const TABLE_W: &str = "do_w";
    /// 不干渉確認用の第 2 テナント。
    const OTHER_TENANT: &str = "crash-tool-sequence-other";
    /// 第 2 テナントの種行の `k`（反復番号とは重ならない負値）。
    const SEED_K: i64 = -1;
    /// 1 反復が `do_a` に書く行数。
    const ROWS_PER_K: u64 = 3;

    fn other_ctx() -> Result<PolicyContext, String> {
        PolicyContext::with_visibilities(OTHER_TENANT, [Visibility::Public, Visibility::Private])
            .map_err(|e| format!("invalid tenant context: {e}"))
    }

    /// 表と第 2 テナントの種行を冪等に用意する（再オープン時は重複エラー・既存の種行を無視する）。
    fn ensure_schema(core: &EngineCore) -> Result<(), String> {
        let ctx = tenant_ctx()?;
        let mut session = SessionState::default();
        session.allow_ddl();
        for sql in [
            format!("CREATE TABLE {TABLE_A} (k BIGINT, j INTEGER)"),
            format!("CREATE TABLE {TABLE_B} (k BIGINT)"),
            format!("CREATE TABLE {TABLE_W} (k BIGINT)"),
        ] {
            match core.execute_sql_in_session(&ctx, &mut session, &sql) {
                Ok(_) => {}
                Err(e) if is_already_exists(&e) => {}
                Err(e) => return Err(format!("ddl failed ({sql}): {e}")),
            }
        }
        let other = other_ctx()?;
        let seeded = read_i64(
            core,
            &other,
            &format!("SELECT k FROM {TABLE_A} LIMIT {SCAN_LIMIT}"),
        )?;
        if seeded.is_empty() {
            let mut s = SessionState::default();
            core.execute_sql_in_session(
                &other,
                &mut s,
                &format!("INSERT INTO {TABLE_A} (k, j) VALUES ({SEED_K}, 0)"),
            )
            .map_err(|e| format!("seed failed: {e}"))?;
        }
        Ok(())
    }

    fn cell_i64(cell: Option<&Cell>) -> Result<i64, String> {
        match cell {
            Some(Cell::SignedInteger(v)) => Ok(*v),
            Some(Cell::Integer(v)) => i64::try_from(*v).map_err(|_| "cell overflow".to_string()),
            other => Err(format!("unexpected cell: {other:?}")),
        }
    }

    /// 1 列目の整数を全件読む（`SCAN_LIMIT` へ達したら全件を見られないため fail-closed）。
    fn read_i64(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<Vec<i64>, String> {
        let r = core
            .execute_sql(ctx, sql)
            .map_err(|e| format!("scan failed ({sql}): {e}"))?;
        if r.rows.len() as u64 >= SCAN_LIMIT {
            return Err(format!(
                "scan limit ({SCAN_LIMIT}) reached; reduce sets/iterations"
            ));
        }
        r.rows
            .iter()
            .map(|row| cell_i64(row.cells.first()))
            .collect()
    }

    /// 反復 `k` の `DO` 文（本体の文字列へはツール内部の整数だけを埋め込む）。
    fn do_sql(k: u64) -> String {
        format!(
            "DO 'BEGIN INSERT INTO {TABLE_A} (k, j) VALUES ({k}, 1); \
             INSERT INTO {TABLE_A} (k, j) VALUES ({k}, 2); \
             INSERT INTO {TABLE_A} (k, j) VALUES ({k}, 3); \
             INSERT INTO {TABLE_B} (k) VALUES ({k}); END'"
        )
    }

    fn witness_sql(k: u64) -> String {
        format!("INSERT INTO {TABLE_W} (k) VALUES ({k}) USING OPERATION_ID 'do-{k}'")
    }

    /// 反復 `k` を適用する。偶数は autocommit の `DO`、奇数は明示 Tx の `DO` + 証人 INSERT。
    fn apply_k(core: &EngineCore, ctx: &PolicyContext, k: u64) -> Result<(), String> {
        let mut session = SessionState::default();
        if k.is_multiple_of(2) {
            core.execute_sql_in_session(ctx, &mut session, &do_sql(k))
                .map_err(|e| format!("do failed at k={k}: {e}"))?;
        } else {
            let mut txn = core.new_session_transaction();
            for stmt in [
                "BEGIN".to_string(),
                do_sql(k),
                witness_sql(k),
                "COMMIT".to_string(),
            ] {
                core.execute_sql_in_txn(ctx, &mut session, &mut txn, &stmt)
                    .map_err(|e| format!("txn statement failed at k={k}: {e}"))?;
            }
        }
        Ok(())
    }

    /// オラクル本体（純関数）。`a` = `do_a` の `(k, j)`、`b` = `do_b` の `k`、`w` = `do_w` の `k`、
    /// `ledger` = 台帳に記録済みの `k`。戻り値は `(do_a の行数, 適用済みの k の個数)`。
    pub(super) fn check_state(
        a: &[(i64, i64)],
        b: &[i64],
        w: &[i64],
        ledger: &[i64],
    ) -> Result<(u64, i64), String> {
        use std::collections::BTreeMap;
        let mut per_k: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (k, j) in a {
            per_k.entry(*k).or_default().push(*j);
        }
        let mut per_b: BTreeMap<i64, u64> = BTreeMap::new();
        for k in b {
            *per_b.entry(*k).or_default() += 1;
        }
        // (1) 全か無か: do_a がちょうど j=1..=3 ⟺ do_b がちょうど 1 行。
        for (k, js) in &per_k {
            let mut js = js.clone();
            js.sort_unstable();
            if js != [1, 2, 3] {
                return Err(format!("partial do_a rows at k={k} (count={})", js.len()));
            }
            if per_b.get(k) != Some(&1) {
                return Err(format!("do_a without exactly one do_b row at k={k}"));
            }
        }
        if let Some(k) = per_b.keys().find(|k| !per_k.contains_key(k)) {
            return Err(format!("do_b row without do_a rows at k={k}"));
        }
        // (2) 適用済みの k は 0 から連続する接頭辞。
        let applied = i64::try_from(per_k.len()).map_err(|_| "overflow".to_string())?;
        if per_k.keys().copied().ne(0..applied) {
            return Err("applied k values are not a contiguous prefix from 0".to_string());
        }
        // (3) 台帳・証人・本体の書き込みが一致する（奇数 k のみ）。
        for k in 0..applied {
            let witnessed = w.iter().filter(|x| **x == k).count();
            let recorded = ledger.contains(&k);
            let expect = k % 2 == 1;
            if witnessed != usize::from(expect) || recorded != expect {
                return Err(format!(
                    "ledger/witness/body mismatch at k={k} (witness={witnessed}, recorded={recorded})"
                ));
            }
        }
        if w.iter().chain(ledger).any(|k| *k >= applied || *k < 0) {
            return Err("witness or ledger entry without an applied body".to_string());
        }
        let rows = u64::try_from(applied).map_err(|_| "overflow".to_string())? * ROWS_PER_K;
        Ok((rows, applied))
    }

    fn read_a(core: &EngineCore, ctx: &PolicyContext) -> Result<Vec<(i64, i64)>, String> {
        let r = core
            .execute_sql(
                ctx,
                &format!("SELECT k, j FROM {TABLE_A} LIMIT {SCAN_LIMIT}"),
            )
            .map_err(|e| format!("scan failed: {e}"))?;
        if r.rows.len() as u64 >= SCAN_LIMIT {
            return Err(format!(
                "scan limit ({SCAN_LIMIT}) reached; reduce sets/iterations"
            ));
        }
        r.rows
            .iter()
            .map(|row| Ok((cell_i64(row.cells.first())?, cell_i64(row.cells.get(1))?)))
            .collect()
    }

    fn recorded(core: &EngineCore, ctx: &PolicyContext, k: i64) -> Result<bool, String> {
        use engine::recovery::ledger::LedgerLookup;
        use engine::recovery::required_op_id::OperationId;
        let op = OperationId::parse(&format!("do-{k}")).map_err(|e| format!("op id: {e:?}"))?;
        match core.operation_recorded(ctx, TABLE_W, &op) {
            Ok(LedgerLookup::Recorded) => Ok(true),
            Ok(LedgerLookup::NotRecorded) => Ok(false),
            Ok(LedgerLookup::NoLedger) => Err("ledger unavailable".to_string()),
            Err(e) => Err(format!("ledger lookup failed: {e}")),
        }
    }

    fn verify_with(core: &EngineCore, ctx: &PolicyContext) -> Result<(u64, i64), String> {
        // 書き込みが走っていない前提（kill 後・finish 中）の連続 read。
        let a_all = read_a(core, ctx)?;
        let b_all = read_i64(
            core,
            ctx,
            &format!("SELECT k FROM {TABLE_B} LIMIT {SCAN_LIMIT}"),
        )?;
        let w_all = read_i64(
            core,
            ctx,
            &format!("SELECT k FROM {TABLE_W} LIMIT {SCAN_LIMIT}"),
        )?;
        // 台帳は奇数 k を、適用済みの個数 + 2 まで探る（未適用の k が記録されていれば検出する）。
        let probe_to = i64::try_from(a_all.len() as u64 / ROWS_PER_K + 2)
            .map_err(|_| "overflow".to_string())?;
        let mut ledger = Vec::new();
        for k in (1..probe_to).step_by(2) {
            if recorded(core, ctx, k)? {
                ledger.push(k);
            }
        }
        let state = check_state(&a_all, &b_all, &w_all, &ledger)?;
        // (4) 第 2 テナントは種行のまま、台帳に do-* が載っていない。
        let other = other_ctx()?;
        let seen = read_i64(
            core,
            &other,
            &format!("SELECT k FROM {TABLE_A} LIMIT {SCAN_LIMIT}"),
        )?;
        if seen != [SEED_K] {
            return Err(format!("other tenant rows changed (count={})", seen.len()));
        }
        for k in 0..probe_to {
            if recorded(core, &other, k)? {
                return Err("other tenant ledger holds a do-* entry".to_string());
            }
        }
        Ok(state)
    }

    pub(super) fn verify_inner(path: &str) -> Result<(u64, i64), String> {
        let core = open_core(path)?;
        verify_with(&core, &tenant_ctx()?)
    }

    pub(super) fn write_inner(path: &str) -> Result<(), String> {
        let core = open_core(path)?;
        ensure_schema(&core)?;
        let ctx = tenant_ctx()?;
        // 再開: verify が接頭辞を保証しているため、既存の行数から次の k を決める。
        let first = u64::try_from(read_a(&core, &ctx)?.len())
            .map_err(|_| "overflow".to_string())?
            / ROWS_PER_K;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        for k in first..first.saturating_add(MAX_ROWS) {
            apply_k(&core, &ctx, k)?;
            let total = (k + 1) * ROWS_PER_K;
            writeln!(handle, "COMMITTED op=do-{k} rows={total} n={}", k + 1)
                .map_err(|e| format!("stdout write failed: {e}"))?;
            handle
                .flush()
                .map_err(|e| format!("stdout flush failed: {e}"))?;
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        Ok(())
    }

    pub(super) fn finish_inner(path: &str) -> Result<(u64, i64), String> {
        let core = open_core(path)?;
        ensure_schema(&core)?;
        let ctx = tenant_ctx()?;
        let (mut rows, mut max) = verify_with(&core, &ctx)?;
        // 再送対象の奇数 k が無いほど短い実行でも検証が空振りしないよう、足りない分を適用する。
        while max < 2 {
            apply_k(
                &core,
                &ctx,
                u64::try_from(max).map_err(|_| "overflow".to_string())?,
            )?;
            (rows, max) = verify_with(&core, &ctx)?;
        }
        let applied = u64::try_from(max).map_err(|_| "overflow".to_string())?;
        let last_odd = if (applied - 1) % 2 == 1 {
            applied - 1
        } else {
            applied - 2
        };

        // (i) 順序 B（DO が先、続く証人 INSERT が台帳で拒否される）の再送は全体が破棄される。
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
            .map_err(|e| format!("begin failed: {e}"))?;
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &do_sql(last_odd))
            .map_err(|e| format!("resend do failed: {e}"))?;
        match core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &witness_sql(last_odd)) {
            Err(SqlSurfaceError::DuplicateOperationId) => {}
            Err(e) => return Err(format!("resend failed with an unexpected error: {e}")),
            Ok(_) => return Err("resend of a committed operation_id was wrongly accepted".into()),
        }
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "ROLLBACK")
            .map_err(|e| format!("rollback failed: {e}"))?;
        let after = verify_with(&core, &ctx)?;
        if after != (rows, max) {
            return Err(format!(
                "resend changed the tables: before=({rows},{max}) after=({},{})",
                after.0, after.1
            ));
        }
        // (ii) `DO ... USING OPERATION_ID` は受理せず（0A000）、何も書かない。
        let mut s2 = SessionState::default();
        let sql = format!("{} USING OPERATION_ID 'do-x'", do_sql(applied));
        match core.execute_sql_in_session(&ctx, &mut s2, &sql) {
            Err(e) if e.wire_code() == "0A000" => {}
            Err(e) => return Err(format!("unexpected error for DO USING OPERATION_ID: {e}")),
            Ok(_) => return Err("DO USING OPERATION_ID was wrongly accepted".to_string()),
        }
        if verify_with(&core, &ctx)? != (rows, max) {
            return Err("rejected DO USING OPERATION_ID changed the tables".to_string());
        }
        // (iii) 新しい k がちょうど 1 回分だけ適用される。
        apply_k(&core, &ctx, applied)?;
        let done = verify_with(&core, &ctx)?;
        if done != (rows + ROWS_PER_K, max + 1) {
            return Err(format!("unexpected state after the new k: {done:?}"));
        }
        Ok(done)
    }

    pub(super) fn run_self_check(path: &str) -> i32 {
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
        let full = |k: i64| [(k, 1), (k, 2), (k, 3)];
        // 妥当な状態（k=0 は autocommit、k=1 は証人と台帳つき）。
        let a: Vec<(i64, i64)> = full(0).into_iter().chain(full(1)).collect();
        if check_state(&a, &[0, 1], &[1], &[1]) != Ok((6, 2)) {
            return Err("oracle rejected a valid state".to_string());
        }
        type Case = (&'static str, Vec<(i64, i64)>, Vec<i64>, Vec<i64>, Vec<i64>);
        let bad: Vec<Case> = vec![
            ("do_a only", full(0).to_vec(), vec![], vec![], vec![]),
            ("do_b only", vec![], vec![0], vec![], vec![]),
            (
                "partial do_a",
                vec![(0, 1), (0, 2)],
                vec![0],
                vec![],
                vec![],
            ),
            (
                "duplicate do_b",
                full(0).to_vec(),
                vec![0, 0],
                vec![],
                vec![],
            ),
            ("gap in k", full(1).to_vec(), vec![1], vec![1], vec![1]),
            (
                "ledger without witness",
                a.clone(),
                vec![0, 1],
                vec![],
                vec![1],
            ),
            (
                "witness without ledger",
                a.clone(),
                vec![0, 1],
                vec![1],
                vec![],
            ),
            (
                "ledger without body effect",
                full(0).to_vec(),
                vec![0],
                vec![],
                vec![1],
            ),
            (
                "duplicate witness",
                a.clone(),
                vec![0, 1],
                vec![1, 1],
                vec![1],
            ),
        ];
        for (label, a, b, w, ledger) in &bad {
            if check_state(a, b, w, ledger).is_ok() {
                return Err(format!("oracle accepted a bad state: {label}"));
            }
        }
        // DB 経由: do_a だけが書かれた状態を verify 本体が検出する。
        let core = open_core(path)?;
        ensure_schema(&core)?;
        let ctx = tenant_ctx()?;
        let mut session = SessionState::default();
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("INSERT INTO {TABLE_A} (k, j) VALUES (0, 1)"),
        )
        .map_err(|e| format!("self-check insert failed: {e}"))?;
        if verify_with(&core, &ctx).is_ok() {
            return Err("oracle accepted a partial state written to the database".to_string());
        }
        Ok(())
    }
}

fn run_write_do(path: &str) -> i32 {
    match do_block::write_inner(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}
