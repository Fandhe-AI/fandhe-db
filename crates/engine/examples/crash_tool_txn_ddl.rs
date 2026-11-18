//! Issue #3318（親 TASK-349。ビヘイビア SQL-80・SQL-31・SQL-23・SQL-52・TABLE-22・TABLE-36・TABLE-44。
//! 関連 PERSIST-1・TABLE-10。ポインタ: `docs/spec/05-tasks.md`）の「DDL を含む明示トランザクション」
//! クラッシュ耐性回帰テスト用ツール。`scripts/crash_test_txn_kill_points.sh`（Makefile の
//! `crash-test-txn-kill-points`・CI の `crash-test-cross-table` ジョブの 1 step から呼ばれる）が
//! `write` / `write-pause` をバックグラウンド起動して SIGKILL し、続けて `verify` で再オープン後の
//! 整合を検証する。
//!
//! 既存の `crash_tool_ddl.rs`（1 文の DDL・autocommit）と `crash_tool_cross_table.rs`（DML だけの
//! 明示 Tx）が覆わない「DDL と DML を混ぜた複数文の明示 Tx」を担当する。engine の本番コードは変えず、
//! 公開 API（`EngineCore::execute_sql_in_txn`）だけで Tx を組み、kill 後のカタログ・行・索引宣言・
//! 索引の物理構造・同じ Tx で書いた既存表（台帳 `td_log`）の行が「全部ある」か「全部ない」かの
//! どちらか一方であること（部分コミット 0 件）を確かめる。
//!
//! Tx k（0 起点の通番。SQL は整数の k と定数だけから組み立てる）の文列:
//! `BEGIN` / `CREATE TABLE td_<k> (k BIGINT, v TEXT)` / `CREATE UNIQUE INDEX td_<k>_k`（空の表）/
//! `INSERT`（SEED 行）[in-txn] / `CREATE INDEX td_<k>_v`（スカラー）/ `INSERT INTO td_log`（Tx の前から
//! 在る表への DML）[before-commit] / `COMMIT` [after-commit]。
//!
//! サブコマンド:
//! - `write <db>`: 再開時の整合を確かめ（不整合なら開始を拒否）、Tx を自走する。最初の DDL の成功直後に
//!   `IN_TXN txn=<k>`、`COMMIT` を送る直前（間に待機を入れない）に `COMMIT_SENT txn=<k>`、`COMMIT` 成功後に
//!   `COMMITTED txn=<k> txns=<n>` を出す。
//! - `write-pause <db> <in-txn|before-commit|after-commit> <warmup>`: warmup 件の完全な Tx の後、次の Tx の
//!   指定点で `PAUSED point=<p> txn=<k> expect_txns=<m>` を出して停止する（SIGKILL 待ち）。
//! - `verify <db>`: `RESULT ok=true txns=<S>` または `RESULT ok=false reason=<...>`。
//! - `self-check <workdir>`: 負の対照。到達できない状態（DDL だけ・行だけ・台帳行欠落・索引の過不足・
//!   物理表の残骸・空 DB 等）を verify が必ず拒否することを確かめる。
//!
//! 同期点を `commit_active` へ足して COMMIT の内部で決定的に止める方式は採らない（本番コードの変更と
//! feature 付きの別ビルドが要るため）。redb commit の前後は `before-commit`・`after-commit` で毎回
//! 決定的に、COMMIT の実行中は `write` のランダム kill で確率的に覆う。
//!
//! 失敗メッセージには `wire_code`・表名・件数だけを含め、行の値・他テナントの存在情報は出さない。

// 許可（clippy::disallowed_methods）: 本ファイルは engine の公開 API の外から redb を直接開き、
// 永続内容の検査・残骸の裏口の対照を行う。変更ログの捕捉（`storage::change_log`）を通す対象は
// engine 製品コードの書き込み点で、外部クレートとしての直接操作は対象外。
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use redb::{ReadableDatabase, TableHandle};

/// Tx を実行するテナントと、境界確認用の別テナント。
const TENANT_A: &str = "crash-txd-a";
const TENANT_B: &str = "crash-txd-b";
/// 表 `td_<k>` へ投入する行数（k = 1..=SEED）。
const SEED: i64 = 8;
/// 自走する Tx 数の上限（安全弁。表の増加も抑える）。
const MAX_TXNS: u64 = 2_000;
/// `SELECT … LIMIT` の上限。達したら全件を見られないので fail-closed。
const SCAN_LIMIT: u64 = 10_000;
/// 文間の待機（ミリ秒）。`COMMIT_SENT` と `COMMIT` の間には入れない。
const STMT_PAUSE_MS: u64 = 2;
/// `write-pause` の一時停止の上限秒数。SIGKILL が来なければ非 0 終了する（無限待ちしない）。
const PAUSE_LIMIT_SECS: u64 = 60;

type Res<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["write", path] => run_write(path),
        ["write-pause", path, point, warmup] => run_write_pause(path, point, warmup),
        ["verify", path] => run_verify(path),
        ["self-check", dir] => run_self_check(dir),
        _ => {
            eprintln!(
                "usage: crash_tool_txn_ddl <write <db>|write-pause <db> <in-txn|before-commit|after-commit> <warmup>|verify <db>|self-check <workdir>>"
            );
            2
        }
    };
    std::process::exit(code);
}

// ---------- 文列（write・verify・self-check で共有する） ----------

/// Tx k の `BEGIN` と `COMMIT` の間の文。添字 2 の直後が in-txn 点、末尾の直後が before-commit 点。
fn body_sqls(k: u64) -> Vec<String> {
    let vals: Vec<String> = (1..=SEED).map(|i| format!("({i}, 'r{k}-{i}')")).collect();
    vec![
        format!("CREATE TABLE td_{k} (k BIGINT, v TEXT)"),
        format!("CREATE UNIQUE INDEX td_{k}_k ON td_{k} (k)"),
        format!("INSERT INTO td_{k} (k, v) VALUES {}", vals.join(", ")),
        format!("CREATE INDEX td_{k}_v ON td_{k} (v)"),
        format!("INSERT INTO td_log (seq) VALUES ({k})"),
    ]
}

/// Tx 内の観測点。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Point {
    /// 最初の DDL の成功直後。
    FirstDdl,
    /// 行の `INSERT` の成功直後。
    InTxn,
    /// 全文の成功後・`COMMIT` を送る直前。
    BeforeCommit,
    /// `COMMIT` が `Ok` を返した直後。
    AfterCommit,
}

// ---------- 共通ヘルパー ----------

fn ctx(tenant: &str) -> Res<PolicyContext> {
    // SQL 表層の INSERT は可視性を Private 固定で書くため、両方を許可する。
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn open_core(path: &str) -> Res<EngineCore> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

fn sql_head(sql: &str) -> String {
    sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// 文を autocommit で実行する。失敗は `wire_code` だけを返す（文の中身・行データは含めない）。
fn try_exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let c = ctx(tenant).map_err(|_| "ctx".to_string())?;
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Res<SqlOutcome> {
    try_exec(core, tenant, sql)
        .map_err(|code| format!("statement failed ({code}): {}", sql_head(sql)))
}

/// 期待する `wire_code` で失敗することを確かめる。成功・別コードは失敗にする（握りつぶさない）。
fn expect_code(core: &EngineCore, tenant: &str, sql: &str, want: &str, what: &str) -> Res<()> {
    match try_exec(core, tenant, sql) {
        Err(code) if code == want => Ok(()),
        Err(code) => Err(format!("{what}: unexpected wire_code {code} (want {want})")),
        Ok(_) => Err(format!("{what}: statement was wrongly accepted")),
    }
}

fn int_of(c: &Cell) -> Option<i64> {
    match c {
        Cell::SignedInteger(i) => Some(*i),
        Cell::Integer(u) => i64::try_from(*u).ok(),
        _ => None,
    }
}

fn text_of(c: &Cell) -> Option<&str> {
    match c {
        Cell::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

/// `SELECT` の結果の行（セル列）を返す。上限に達したら fail-closed。
fn select(core: &EngineCore, tenant: &str, sql: &str) -> Res<Vec<Vec<Cell>>> {
    match exec(core, tenant, sql)? {
        SqlOutcome::Query(q) => {
            if q.rows.len() as u64 >= SCAN_LIMIT {
                return Err("scan limit reached; cannot see all rows".to_string());
            }
            Ok(q.rows.into_iter().map(|r| r.cells).collect())
        }
        _ => Err("expected a query outcome".to_string()),
    }
}

/// stdout へ 1 行書き切って flush する（kill 時に行が途中で切れる窓を最小化する）。
fn emit_line(line: &str) -> Res<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{line}").map_err(|e| format!("stdout write failed: {e}"))?;
    out.flush().map_err(|e| format!("stdout flush failed: {e}"))
}

/// 台帳表 `td_log` を（無ければ）作る。冪等（既存は 42P07 を許容）。
fn ensure_log(core: &EngineCore) -> Res<()> {
    match try_exec(core, TENANT_A, "CREATE TABLE td_log (seq BIGINT)") {
        Ok(_) => Ok(()),
        Err(code) if code == "42P07" => Ok(()),
        Err(code) => Err(format!("ensure td_log failed ({code})")),
    }
}

// ---------- 明示 Tx 実行器 ----------

/// 失敗時は ROLLBACK を試みて閉じる明示 Tx 実行器（DDL 許可セッション）。
struct TxnSession<'a> {
    core: &'a EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'a>,
}

impl<'a> TxnSession<'a> {
    fn begin(core: &'a EngineCore, tenant: &str) -> Res<Self> {
        let ctx = ctx(tenant)?;
        let mut session = SessionState::default();
        session.allow_ddl();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
            .map_err(|e| format!("BEGIN failed ({})", e.wire_code()))?;
        Ok(Self {
            core,
            ctx,
            session,
            txn,
        })
    }

    /// 文を実行する。失敗したら ROLLBACK を試みてから `Err` を返す。
    fn run(&mut self, sql: &str) -> Res<SqlOutcome> {
        match self
            .core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
        {
            Ok(o) => Ok(o),
            Err(e) => {
                let _ = self.core.execute_sql_in_txn(
                    &self.ctx,
                    &mut self.session,
                    &mut self.txn,
                    "ROLLBACK",
                );
                Err(format!("{} failed ({})", sql_head(sql), e.wire_code()))
            }
        }
    }
}

/// Tx k を 1 つ実行し、各観測点で `hook` を呼ぶ。`pause` が真なら文と文の間に短く待つ
/// （`BeforeCommit` の hook と `COMMIT` の間には待機を入れない）。
fn run_txn(
    core: &EngineCore,
    k: u64,
    pause: bool,
    hook: &mut dyn FnMut(Point, u64) -> Res<()>,
) -> Res<()> {
    let mut t = TxnSession::begin(core, TENANT_A)?;
    let sleep = || {
        if pause {
            std::thread::sleep(std::time::Duration::from_millis(STMT_PAUSE_MS));
        }
    };
    for (i, sql) in body_sqls(k).iter().enumerate() {
        t.run(sql)?;
        match i {
            0 => hook(Point::FirstDdl, k)?,
            2 => hook(Point::InTxn, k)?,
            _ => {}
        }
        if i + 1 < 5 {
            sleep();
        }
    }
    hook(Point::BeforeCommit, k)?;
    t.run("COMMIT")?;
    hook(Point::AfterCommit, k)
}

// ---------- write ----------

fn run_write(path: &str) -> i32 {
    match write(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: write failed: {reason}");
            1
        }
    }
}

fn write(path: &str) -> Res<()> {
    let core = open_core(path)?;
    ensure_log(&core)?;
    drop(core);
    // 再開: 状態が整合していなければ fail-closed で開始を拒否する。
    let start = check(path, true).map_err(|e| format!("inconsistent state on resume: {e}"))?;
    let core = open_core(path)?;
    for n in 0..MAX_TXNS {
        let k = start
            .checked_add(n)
            .ok_or_else(|| "txn seq overflow".to_string())?;
        run_txn(&core, k, true, &mut |p, k| match p {
            Point::FirstDdl => emit_line(&format!("IN_TXN txn={k}")),
            Point::BeforeCommit => emit_line(&format!("COMMIT_SENT txn={k}")),
            Point::AfterCommit => {
                let total = k
                    .checked_add(1)
                    .ok_or_else(|| "txn count overflow".to_string())?;
                emit_line(&format!("COMMITTED txn={k} txns={total}"))
            }
            Point::InTxn => Ok(()),
        })?;
    }
    Ok(())
}

fn run_write_pause(path: &str, point: &str, warmup: &str) -> i32 {
    let p = match point {
        "in-txn" => Point::InTxn,
        "before-commit" => Point::BeforeCommit,
        "after-commit" => Point::AfterCommit,
        _ => return usage(),
    };
    let Ok(warmup) = warmup.parse::<u64>() else {
        return usage();
    };
    if warmup == 0 || warmup > MAX_TXNS {
        return usage();
    }
    match write_pause(path, p, warmup) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

fn usage() -> i32 {
    eprintln!("usage: crash_tool_txn_ddl write-pause <db> <in-txn|before-commit|after-commit> <warmup 1..={MAX_TXNS}>");
    2
}

/// `warmup` 件の完全な Tx を commit したのち、次の Tx の指定点で `PAUSED` を出して停止する。
/// 期待値は途中・COMMIT 直前では「その Tx は全く残らない」、COMMIT 直後では「全部残る」
/// （COMMIT は redb の Immediate commit を同期的に行うため）。
fn write_pause(path: &str, point: Point, warmup: u64) -> Res<()> {
    let core = open_core(path)?;
    ensure_log(&core)?;
    drop(core);
    let start = check(path, true).map_err(|e| format!("inconsistent state on resume: {e}"))?;
    let core = open_core(path)?;
    let target = start
        .checked_add(warmup)
        .ok_or_else(|| "txn seq overflow".to_string())?;
    for k in start..target {
        run_txn(&core, k, false, &mut |p, k| {
            if p != Point::AfterCommit {
                return Ok(());
            }
            let total = k
                .checked_add(1)
                .ok_or_else(|| "txn count overflow".to_string())?;
            emit_line(&format!("COMMITTED txn={k} txns={total}"))
        })?;
    }
    let (name, expect) = match point {
        Point::AfterCommit => ("after-commit", target.checked_add(1)),
        Point::BeforeCommit => ("before-commit", Some(target)),
        _ => ("in-txn", Some(target)),
    };
    let expect = expect.ok_or_else(|| "txn seq overflow".to_string())?;
    run_txn(&core, target, false, &mut |p, k| {
        if p != point {
            return Ok(());
        }
        emit_line(&format!("PAUSED point={name} txn={k} expect_txns={expect}"))?;
        std::thread::sleep(std::time::Duration::from_secs(PAUSE_LIMIT_SECS));
        Err("pause limit reached without SIGKILL".to_string())
    })
}

// ---------- 観測 ----------

/// カタログの観測値（`Storage` を開いて読み、すぐ閉じる。`EngineCore` と同時には開かない）。
struct Catalog {
    tables: Vec<String>,
    columns: BTreeMap<String, Vec<String>>,
    /// （索引名, 表名, スカラーか, 一意か, 列）。
    indexes: Vec<(String, String, bool, bool, Vec<String>)>,
    /// raw redb の物理表名（一意索引の物理構造 `user_uniq/<表>` の有無の判定に使う）。
    physical: BTreeSet<String>,
}

impl Catalog {
    fn read(path: &str) -> Res<Catalog> {
        let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
        let tables = storage
            .list_tables()
            .map_err(|e| format!("list tables failed: {e}"))?;
        let mut columns = BTreeMap::new();
        for t in &tables {
            let schema = storage
                .get_table_schema(t)
                .map_err(|e| format!("get schema failed: table={t}: {e}"))?;
            columns.insert(
                t.clone(),
                schema.columns.iter().map(|c| c.name.clone()).collect(),
            );
        }
        let indexes = storage
            .list_indexes()
            .map_err(|e| format!("list indexes failed: {e}"))?
            .into_iter()
            .map(|d| {
                (
                    d.name,
                    d.table,
                    matches!(d.kind, engine::catalog::IndexKind::Scalar),
                    d.unique,
                    d.columns,
                )
            })
            .collect();
        drop(storage);
        // Storage を閉じてから raw redb の物理表名を読む。
        let physical = physical_tables(path)?;
        Ok(Catalog {
            tables,
            columns,
            indexes,
            physical,
        })
    }
}

/// raw redb の物理表名の一覧。
fn physical_tables(path: &str) -> Res<BTreeSet<String>> {
    let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
    let r = db.begin_read().map_err(|_| "raw read failed".to_string())?;
    let tables = r
        .list_tables()
        .map_err(|_| "raw list tables failed".to_string())?;
    Ok(tables.map(|h| h.name().to_string()).collect())
}

/// `td_<数字>` の表名から通番を取り出す（それ以外は `None`）。
fn parse_txn(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("td_")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// 物理表名のセグメント（`/` 区切り）が `td_<j>`・`td_<j>_k`・`td_<j>_v` のとき `<j>` を返す。
/// 接頭辞を決め打ちしない（`td_1` と `td_10` を取り違えない）。
fn seg_txn(seg: &str) -> Option<u64> {
    let rest = seg.strip_prefix("td_")?;
    let num = rest
        .strip_suffix("_v")
        .or_else(|| rest.strip_suffix("_k"))
        .unwrap_or(rest);
    num.parse().ok()
}

// ---------- verify ----------

fn run_verify(path: &str) -> i32 {
    match check(path, false) {
        Ok(s) => {
            println!("RESULT ok=true txns={s}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

/// 状態を検査し、commit 済みの Tx 数 S を返す。期待に無い表・列・索引・行・物理表は即失敗（fail-closed）。
/// `allow_zero` が偽のとき S = 0（空虚な成功）も失敗にする。
fn check(path: &str, allow_zero: bool) -> Res<u64> {
    let cat = Catalog::read(path)?;
    let mut txns: BTreeSet<u64> = BTreeSet::new();
    let mut has_log = false;
    for name in &cat.tables {
        if name == "td_log" {
            has_log = true;
        } else if let Some(j) = parse_txn(name) {
            txns.insert(j);
        } else {
            return Err(format!("unexpected table in catalog: {name}"));
        }
    }
    if !has_log {
        return Err("ledger table missing".to_string());
    }
    let s = u64::try_from(txns.len()).map_err(|_| "count overflow".to_string())?;
    if txns.iter().copied().ne(0..s) {
        return Err(format!(
            "table set is not a contiguous txn prefix: count={s}"
        ));
    }
    if s == 0 && !allow_zero {
        return Err("no transaction applied (vacuous)".to_string());
    }
    let cols_of = |t: &str| cat.columns.get(t).map(Vec::as_slice).unwrap_or(&[]);
    if cols_of("td_log") != ["seq"] {
        return Err("unexpected columns: table=td_log".to_string());
    }
    for j in 0..s {
        if cols_of(&format!("td_{j}")) != ["k", "v"] {
            return Err(format!("unexpected columns: table=td_{j}"));
        }
    }
    // 索引宣言は各 Tx の 2 件（一意 k・スカラー v）ちょうど。過不足・別名は失敗。
    let mut seen: BTreeSet<(u64, bool)> = BTreeSet::new();
    for (name, table, scalar, unique, columns) in &cat.indexes {
        let j = parse_txn(table)
            .filter(|j| *j < s)
            .ok_or_else(|| format!("index of a missing table: table={table}"))?;
        let is_k = *name == format!("td_{j}_k") && *unique && columns == &["k"];
        let is_v = *name == format!("td_{j}_v") && *scalar && !*unique && columns == &["v"];
        if !(is_k || is_v) || !seen.insert((j, is_k)) {
            return Err(format!("unexpected index declaration: table={table}"));
        }
    }
    if seen.len() as u64 != 2 * s {
        return Err(format!(
            "index declarations incomplete: want={} got={}",
            2 * s,
            seen.len()
        ));
    }
    {
        let core = open_core(path)?;
        verify_contents(&core, &cat, s)?;
    }
    // EngineCore を閉じてから raw redb を読む（未 commit の Tx の物理表の残骸）。
    let residue = physical_tables(path)?
        .iter()
        .filter(|name| name.split('/').filter_map(seg_txn).any(|j| j >= s))
        .count();
    if residue > 0 {
        return Err(format!(
            "physical tables of an uncommitted txn remain: count={residue}"
        ));
    }
    Ok(s)
}

fn verify_contents(core: &EngineCore, cat: &Catalog, s: u64) -> Res<()> {
    // 読み取り専用の一意索引検証を、あらゆる書き込み系プローブより先に通す。物理構造は宣言後の
    // 最初の書き込みで遅延構築されるため、未構築の間は検証対象外（宣言の有無と重複プローブの
    // `23505` が宣言の生存を保証する）。
    for j in 0..s {
        if cat.physical.contains(&format!("user_uniq/td_{j}")) {
            core.verify_unique_index_read_only(&ctx(TENANT_A)?, &format!("td_{j}"))
                .map_err(|e| format!("unique index verification failed: table=td_{j}: {e}"))?;
        }
    }
    // 台帳: seq = 0..S-1 の連番ちょうど（DDL と同じ Tx の DML が運命を共にする）。
    let log = select(
        core,
        TENANT_A,
        &format!("SELECT seq FROM td_log ORDER BY seq LIMIT {SCAN_LIMIT}"),
    )?;
    let seqs: Vec<Option<i64>> = log.iter().map(|r| r.first().and_then(int_of)).collect();
    let want: Vec<Option<i64>> = (0..s)
        .map(|i| i64::try_from(i).ok())
        .collect::<Vec<Option<i64>>>();
    if seqs != want {
        return Err(format!(
            "ledger rows do not match tables: ledger={} tables={s}",
            seqs.len()
        ));
    }
    for j in 0..s {
        let name = format!("td_{j}");
        let rows = select(
            core,
            TENANT_A,
            &format!("SELECT k, v FROM {name} ORDER BY k LIMIT {SCAN_LIMIT}"),
        )?;
        if rows.len() as i64 != SEED {
            return Err(format!(
                "unexpected row count: table={name} count={}",
                rows.len()
            ));
        }
        for (i, cells) in rows.iter().enumerate() {
            let k = i64::try_from(i).map_err(|_| "index overflow".to_string())? + 1;
            let ok_k = cells.first().and_then(int_of) == Some(k);
            let ok_v = cells.get(1).and_then(text_of) == Some(format!("r{j}-{k}").as_str());
            if !(ok_k && ok_v) {
                return Err(format!("row content mismatch: table={name} k={k}"));
            }
        }
        // スカラー索引が効くクエリの結果が全走査（期待）と一致する。
        let hit = select(
            core,
            TENANT_A,
            &format!("SELECT k FROM {name} WHERE v = 'r{j}-3' LIMIT {SCAN_LIMIT}"),
        )?;
        if hit.len() != 1 || hit.first().and_then(|c| c.first()).and_then(int_of) != Some(3) {
            return Err(format!(
                "scalar index read differs from the full scan: table={name}"
            ));
        }
        // 一意索引: 重複は 23505 で拒否され、行数は変わらない。
        expect_code(
            core,
            TENANT_A,
            &format!("INSERT INTO {name} (k, v) VALUES (3, 'dup')"),
            "23505",
            &format!("duplicate probe on {name}"),
        )?;
        let after = select(
            core,
            TENANT_A,
            &format!("SELECT k FROM {name} LIMIT {SCAN_LIMIT}"),
        )?;
        if after.len() as i64 != SEED {
            return Err(format!(
                "row count changed by a rejected insert: table={name}"
            ));
        }
        // テナント境界: 別テナントからは行が見えない。
        let other = select(
            core,
            TENANT_B,
            &format!("SELECT k FROM {name} LIMIT {SCAN_LIMIT}"),
        )?;
        if !other.is_empty() {
            return Err(format!(
                "rows visible to another tenant: table={name} count={}",
                other.len()
            ));
        }
    }
    Ok(())
}

// ---------- self-check ----------

fn run_self_check(dir: &str) -> i32 {
    match self_check(dir) {
        Ok(()) => {
            println!("SELF-CHECK ok");
            0
        }
        Err(reason) => {
            println!("SELF-CHECK ok=false reason={reason}");
            1
        }
    }
}

fn run_sqls(path: &str, sqls: &[String]) -> Res<()> {
    let core = open_core(path)?;
    for s in sqls {
        exec(&core, TENANT_A, s)?;
    }
    Ok(())
}

fn raw_create(path: &str, name: &str) -> Res<()> {
    let db = redb::Database::open(path).map_err(|_| "raw open failed".to_string())?;
    let w = db
        .begin_write()
        .map_err(|_| "raw write failed".to_string())?;
    {
        let def: redb::TableDefinition<&str, &str> = redb::TableDefinition::new(name);
        w.open_table(def)
            .map_err(|_| "raw create table failed".to_string())?;
    }
    w.commit().map_err(|_| "raw commit failed".to_string())
}

/// 台帳を作り、Tx `ks` を autocommit の文で積む（明示 Tx と同じ最終状態）。`skip` の添字の文は飛ばす。
fn build_auto(path: &str, ks: &[u64], skip: &[usize]) -> Res<()> {
    let mut v = vec!["CREATE TABLE td_log (seq BIGINT)".to_string()];
    for k in ks {
        for (i, s) in body_sqls(*k).into_iter().enumerate() {
            if !skip.contains(&i) {
                v.push(s);
            }
        }
    }
    run_sqls(path, &v)
}

fn self_check(dir: &str) -> Res<()> {
    // 正例: 実際の明示 Tx（DDL 込み）を 2 件 commit した状態が、各 Tx の直後にちょうどその S で通る。
    let pos = format!("{dir}/positive.redb");
    for k in 0..2u64 {
        {
            let core = open_core(&pos)?;
            ensure_log(&core)?;
            run_txn(&core, k, false, &mut |_, _| Ok(()))?;
        }
        let n = check(&pos, false).map_err(|e| format!("positive txn {k} rejected: {e}"))?;
        if n != k + 1 {
            return Err(format!("positive txn {k} inferred txns={n}"));
        }
    }
    // 正例（物理構造あり）: 宣言後に書き込みが 1 度通ると `user_uniq/td_0` が構築される。
    let phys = format!("{dir}/positive-physical.redb");
    build_auto(&phys, &[0], &[])?;
    run_sqls(
        &phys,
        &[
            "INSERT INTO td_0 (k, v) VALUES (100, 'x')".to_string(),
            "DELETE FROM td_0 WHERE k = 100".to_string(),
        ],
    )?;
    if !Catalog::read(&phys)?.physical.contains("user_uniq/td_0") {
        return Err("unique index physical table was not built (vacuous)".to_string());
    }
    if check(&phys, false)? != 1 {
        return Err("positive-physical inferred a wrong txn count".to_string());
    }

    // 負例: Tx の全部でも全無でもない状態は必ず拒否される。
    let neg = |name: &str, build: &dyn Fn(&str) -> Res<()>| -> Res<()> {
        let p = format!("{dir}/neg-{name}.redb");
        build(&p)?;
        match check(&p, false) {
            Ok(_) => Err(format!("oracle accepted an unreachable state: {name}")),
            Err(reason) => {
                println!("NEGATIVE {name}: rejected ({reason})");
                Ok(())
            }
        }
    };
    // 表だけ（索引・行・台帳行なし）。
    neg("table-only", &|p| build_auto(p, &[0], &[1, 2, 3, 4]))?;
    // in-txn 点の状態（表 + 一意索引 + 行。スカラー索引と台帳行なし）。
    neg("in-txn-state", &|p| build_auto(p, &[0], &[3, 4]))?;
    // DDL と行は全部あるが台帳行なし（before-commit 点の台帳だけ欠けた部分コミット）。
    neg("no-ledger-row", &|p| build_auto(p, &[0], &[4]))?;
    // スカラー索引だけ欠ける。
    neg("no-scalar-index", &|p| build_auto(p, &[0], &[3]))?;
    // 台帳行だけ（表なし）。
    neg("ledger-only", &|p| {
        run_sqls(
            p,
            &[
                "CREATE TABLE td_log (seq BIGINT)".to_string(),
                "INSERT INTO td_log (seq) VALUES (0)".to_string(),
            ],
        )
    })?;
    // 行が SEED-1 件。
    neg("missing-row", &|p| {
        build_auto(p, &[0], &[])?;
        run_sqls(p, &["DELETE FROM td_0 WHERE k = 1".to_string()])
    })?;
    // 余分な索引。
    neg("extra-index", &|p| {
        build_auto(p, &[0], &[])?;
        run_sqls(p, &["CREATE INDEX td_0_x ON td_0 (v)".to_string()])
    })?;
    // 通番の欠け（td_0 と td_2）。
    neg("txn-gap", &|p| build_auto(p, &[0, 2], &[]))?;
    // カタログに無い td_1 の物理表（行ストア・一意索引）の残骸。
    neg("residue-rows", &|p| {
        build_auto(p, &[0], &[])?;
        raw_create(p, "user_rows/td_1")
    })?;
    neg("residue-unique", &|p| {
        build_auto(p, &[0], &[])?;
        raw_create(p, "user_uniq/td_1")
    })?;
    // 空の DB（台帳のみ）は空虚な成功として拒否する。
    neg("ledger-only-table", &|p| {
        let core = open_core(p)?;
        ensure_log(&core)
    })?;
    Ok(())
}
