//! Issue #1070（TABLE-16・TASK-204）の永続一意索引・クラッシュ耐性回帰テスト用
//! ツール。`crash_tool_cross_table.rs`（TASK-90）と役割・使い方の形は同じだが、
//! あちらが `engine::txn::BatchWriteTxn`（`ROWS_TABLE`／`BATCH_LOG_TABLE` のみ）を
//! 直叩きするのに対し、本ツールは `EngineCore::execute_insert_sql`（SQL 表層。
//! `constraint::enforce_unique_keys_in_txn` → `unique_index::check_and_update` を
//! 経由する production 経路）を使う——`ROWS_TABLE` は SQL 表層が読み書きしない
//! 旧テーブル（`table_generation_bump_coverage.rs` の ALLOWLIST ドキュメント
//! 参照）であり、TASK-90 のツールは `user_rows/{table}` にも `user_uniq/{table}`
//! （永続一意索引）にも一切触れないため、UNIQUE 制約のクラッシュ耐性検証には
//! 使えない。
//!
//! `scripts/crash_test_unique_index.sh` から `write` サブコマンドをバックグラウンド
//! 起動され SIGKILL される想定、続けて `verify` サブコマンドで再オープン後の
//! 索引と行データの整合性を検証される想定で作られている。
//!
//! Issue #1616（INDEX-9）で、`CREATE UNIQUE INDEX` 由来の一意制約も同じオラクルで検証する
//! ため、2 つ目の表 `docs_uidx (code TEXT)` ＋ `CREATE UNIQUE INDEX IF NOT EXISTS
//! docs_uidx_code ON docs_uidx (code)` を追加した。2 表は独立に id を採番・再開し、`verify` は
//! 両表へ同じオラクルを適用する（`docs` は列制約 `UNIQUE`、`docs_uidx` は索引宣言由来）。
//!
//! Issue #1649（INDEX-10）で、式索引の DML 維持（コミット済みの全行で式が評価できる不変条件）を
//! 検証するため、3 つ目の表 `docs_xidx (n INTEGER, code TEXT)` ＋ 式索引 `((100 / n))`・
//! `((lower(code)))` を追加した。`write` は `n = (id % 7) + 1`（0 にならない）で書き、`verify` は
//! 行 id 連続性・BATCH 整合に加え、式の評価に失敗する INSERT／UPDATE が再起動後も必ず `22012` で
//! 拒否されること（＝宣言と検査点がクラッシュ後も生存している）と、`n = 0` の行が 1 行も無いこと
//! （＝コミット済みの全行が式を評価できる＝派生索引を作り直せる）を確かめる。
//!
//! Issue #1651（INDEX-9・INDEX-10）で、同じ表へ一意の式索引 `((lower(code)))` を加えた。`verify` は
//! 読み取り専用の `verify_unique_index_read_only` が式一意仕様を含めて成功すること（＝永続一意索引の
//! マーカーと全行の正引きエントリがクラッシュ後も生存している）と、既存 code の大文字小文字違いを
//! 新しい id で再挿入すると再起動後も必ず `23505` で拒否されることを確かめる。
//!
//! Issue #1654（INDEX-11）で、部分索引の DML 維持（入場・退場・述語の評価エラーによる拒否が
//! クラッシュ後も行と一致する不変条件）を検証するため、4 つ目の表 `docs_pidx (n INTEGER, code TEXT)`
//! ＋部分索引 2 本（`((100 / n)) WHERE n > 0`・`(code) WHERE 100 / (n + 5) > 1`）を追加した。
//! `write` は `n = (id % 7) - 3`（-3〜3。0 を含み -5 は書かない）で書き、各バッチの直後に先頭行の
//! `n` の符号を反転する UPDATE を打って入退場を kill の対象経路に乗せる。`verify` は、述語の評価に
//! 失敗する INSERT／UPDATE（`n = -5`）が再起動後も必ず `22012` で拒否されること（宣言と検査点の生存）、
//! コミット済みの全行で述語が評価できること（`n = -5` の行が無い）、述語外の `n = 0` の行が永続化
//! されていること（退場側の受理）を確かめる。
//!
//! Issue #1657（INDEX-11 (4)）で、同じ表へ部分 UNIQUE 索引 `(code) WHERE n > 0` を加えた。`code` は
//! `code-{id}` で恒久的に一意なので書き込みは不変で、先頭行の符号反転による入退場が一意索引の正引き・
//! 逆引きの更新を kill の対象経路に乗せる。`verify` は読み取り専用の `verify_unique_index_read_only` が
//! 部分 UNIQUE 仕様を含めて成功すること（永続一意索引がクラッシュ後も述語 TRUE の行と一致する）と、
//! 述語内の既存 code を新しい id・`n > 0` で再挿入すると再起動後も必ず `23505` で拒否されることを確かめる
//! （述語外の重複を受理する側のプローブは行を残してしまうため、結合テストが担当する）。
//!
//! サブコマンド:
//! - `write <db_path>`: `docs (code TEXT UNIQUE)` テーブル（無ければ作成）へ、
//!   `BATCH` 件の行を 1 `INSERT` 文（＝1 write トランザクション。UNIQUE 索引の
//!   更新も同一トランザクション内）でコミットし続ける。`code` 列の値は行 id から
//!   決定的に導出し（`code-{id}`）、id が単調増加である限り恒久的に一意になる。
//!   `COMMITTED batch=<seq> rows=<total>` を stdout へ出力する
//!   （`scripts/crash_test_unique_index.sh` の開始同期点）。再起動時は
//!   `SELECT id FROM docs` で最大 id を求め、そこから採番を再開する。
//! - `verify <db_path>`: 行 id の 0 起点連続性・総行数が `BATCH` の倍数である
//!   ことに加え、**索引の正しさそのもの**を検証する——`docs/design/unique-index.md`
//!   「正しさの不変条件」により、生存する各行が持つキー値は必ず正引きエントリを
//!   持たなければならない。この不変条件が保たれているかは、既存の全 `code` 値を
//!   新しい id で再挿入しようと試み、**必ず `23505`（UNIQUE 制約違反）で拒否される
//!   こと**を確認することで検証する（索引エントリが 1 件でも欠落していれば、
//!   その値だけ誤って受理されてしまう＝クラッシュ後の索引再構築漏れの直接検出）。
//!   受理されてしまった場合は行を残さず `RESULT ok=false` で失敗するが、この
//!   プローブ自体は常に拒否される前提のため、成功時（正しい場合）は副作用を
//!   一切残さない（`write` が再開する id 採番と衝突しない）。

use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

/// write/verify で固定して使うテナント識別子（単一テナントのクラッシュ耐性検証が
/// 目的で、RLS ポリシー評価そのものは対象外。`crash_tool_cross_table.rs` と同方針）。
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// この crash ツールは行 id を行の識別に使う従来方式の契約を検証するため、`CREATE TABLE` を従来方式の
// 表作成へ振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;

const CRASH_TOOL_TENANT_ID: &str = "crash-tool-unique-index-tenant";

/// 1 `INSERT` 文（＝1 write トランザクション）で書き込む行数。行総数はこの値の
/// 倍数になる（verify のバッチ整合オラクル。`crash_tool_cross_table.rs` と同方針）。
const BATCH: u64 = 10;
/// 検証対象の表。`docs` は列制約 `UNIQUE`、`docs_uidx` は `CREATE UNIQUE INDEX` 由来
/// （Issue #1616・INDEX-9）の一意制約を持つ。
const TABLES: [&str; 4] = ["docs", "docs_uidx", XIDX_TABLE, PIDX_TABLE];
/// 式索引（`((100 / n))`・`((lower(code)))`）を持つ表（Issue #1649・INDEX-10）。
const XIDX_TABLE: &str = "docs_xidx";
/// 部分索引（`((100 / n)) WHERE n > 0`・`(code) WHERE 100 / (n + 5) > 1`）を持つ表（Issue #1654・INDEX-11）。
const PIDX_TABLE: &str = "docs_pidx";

/// 再開時に既存行の最大 id を求める `SELECT id FROM docs LIMIT n` の上限件数。
/// `sql::allowlist` が SELECT の `LIMIT` に課す上限（1..=10000）と同じ値を使う
/// （本ツール固有の制限ではなく SQL 表層の既存上限。この値を超える行数は
/// 本ツールでは検証できない——`SCAN_LIMIT` に達した場合は資源制限として
/// fail-closed に拒否する。下記 [`check_scan_not_truncated`] 参照）。
const SCAN_LIMIT: u64 = 10_000;
/// write の自走上限バッチ数（安全弁）。`scripts/crash_test_unique_index.sh` は
/// 書き込み進行中に SIGKILL する想定のため通常はここへ到達しない
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_BATCHES: u64 = 500_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let sub = args.next();
    let path = args.next();
    let (sub, path) = match (sub, path) {
        (Some(sub), Some(path)) => (sub, path),
        _ => {
            eprintln!("usage: crash_tool_unique_index <write|verify> <db_path>");
            std::process::exit(2);
        }
    };

    let exit_code = match sub.as_str() {
        "write" => run_write(&path),
        "verify" => run_verify(&path),
        other => {
            eprintln!("ERROR: unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(exit_code);
}

fn open_core(path: &str) -> Result<EngineCore, String> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

fn tenant_ctx() -> Result<PolicyContext, String> {
    // SQL 表層の `INSERT` は可視性を常に `Visibility::Private` 固定で書き込む
    // （`sql/exec.rs::execute_insert` 系のドキュメント参照）。`Public` のみを
    // 許可するコンテキストだと自分で書いた行が `SELECT` から見えなくなる
    // （テナント自身の可視性フィルタで弾かれる）ため、両方を許可する。
    PolicyContext::with_visibilities(
        CRASH_TOOL_TENANT_ID,
        [Visibility::Public, Visibility::Private],
    )
    .map_err(|e| format!("invalid tenant context: {e}"))
}

/// テーブルが無ければ作成する（既存 DB を再オープンした場合は
/// `SqlSurfaceError::DuplicateTable` を無視して継続する）。`docs_uidx` の一意索引は
/// `IF NOT EXISTS` で冪等に宣言する（Issue #1616・INDEX-9）。
fn ensure_table(core: &EngineCore, ctx: &PolicyContext) -> Result<(), String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    match core.execute_sql_in_session_legacy(
        ctx,
        &mut session,
        "CREATE TABLE docs (code TEXT UNIQUE)",
    ) {
        Ok(_) => {}
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => {}
        Err(e) => return Err(format!("create table failed: {e}")),
    }
    match core.execute_sql_in_session_legacy(
        ctx,
        &mut session,
        "CREATE TABLE docs_uidx (code TEXT)",
    ) {
        Ok(_) => {}
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => {}
        Err(e) => return Err(format!("create table failed: {e}")),
    }
    core.execute_sql_in_session_legacy(
        ctx,
        &mut session,
        "CREATE UNIQUE INDEX IF NOT EXISTS docs_uidx_code ON docs_uidx (code)",
    )
    .map_err(|e| format!("create unique index failed: {e}"))?;
    match core.execute_sql_in_session_legacy(
        ctx,
        &mut session,
        "CREATE TABLE docs_xidx (n INTEGER, code TEXT)",
    ) {
        Ok(_) => {}
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => {}
        Err(e) => return Err(format!("create table failed: {e}")),
    }
    for sql in [
        "CREATE INDEX IF NOT EXISTS docs_xidx_div ON docs_xidx ((100 / n))",
        "CREATE INDEX IF NOT EXISTS docs_xidx_lower ON docs_xidx ((lower(code)))",
        // 一意の式索引（Issue #1651・INDEX-9）。`write` の code は行 id ごとに一意なので常に書ける。
        "CREATE UNIQUE INDEX IF NOT EXISTS docs_xidx_ulower ON docs_xidx ((lower(code)))",
    ] {
        core.execute_sql_in_session_legacy(ctx, &mut session, sql)
            .map_err(|e| format!("create expression index failed: {e}"))?;
    }
    match core.execute_sql_in_session_legacy(
        ctx,
        &mut session,
        "CREATE TABLE docs_pidx (n INTEGER, code TEXT)",
    ) {
        Ok(_) => {}
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => {}
        Err(e) => return Err(format!("create table failed: {e}")),
    }
    for sql in [
        // 述語外の `n <= 0`（`n = 0` を含む）の行は式要素 `100 / n` を評価せずに受理される（退場の意味論）。
        "CREATE INDEX IF NOT EXISTS docs_pidx_div ON docs_pidx ((100 / n)) WHERE n > 0",
        // 述語自体が `n = -5` で 0 除算になる。`write` は -5 を書かない。
        "CREATE INDEX IF NOT EXISTS docs_pidx_pred ON docs_pidx (code) WHERE 100 / (n + 5) > 1",
        // 部分 UNIQUE 索引（Issue #1657）。`code` は恒久的に一意なので `write` は違反しない。
        "CREATE UNIQUE INDEX IF NOT EXISTS docs_pidx_code_u ON docs_pidx (code) WHERE n > 0",
    ] {
        core.execute_sql_in_session_legacy(ctx, &mut session, sql)
            .map_err(|e| format!("create partial index failed: {e}"))?;
    }
    Ok(())
}

/// `write` が `table` の行 `id` に使う `VALUES` の 1 タプル。式索引の表は `n` を `1..=7` にする
/// （0 除算にならない）。
fn row_tuple(table: &str, id: u64) -> String {
    if table == XIDX_TABLE {
        format!("({id}, {}, 'Code-{id}')", id % 7 + 1)
    } else if table == PIDX_TABLE {
        // `n = (id % 7) - 3`（-3〜3。0 を含み、述語が失敗する -5 は書かない）。
        format!(
            "({id}, {}, 'code-{id}')",
            i64::try_from(id % 7).unwrap_or(0) - 3
        )
    } else {
        format!("({id}, 'code-{id}')")
    }
}

/// スキャン結果の件数が [`SCAN_LIMIT`] に達していないことを確認する。達して
/// いれば、それ以上の行が存在するかどうかを本ツールでは判別できない
/// （`LIMIT` で切り捨てられた結果を「全件」として扱うと、resume の採番・
/// verify の索引検証のいずれも取りこぼしを見逃す）ため、資源制限として
/// fail-closed に拒否する。
fn check_scan_not_truncated(row_count: usize) -> Result<(), String> {
    if row_count as u64 >= SCAN_LIMIT {
        return Err(format!(
            "row count reached this tool's scan limit ({SCAN_LIMIT}); reduce the crash \
             test's sets/iterations or extend this tool to paginate the scan"
        ));
    }
    Ok(())
}

/// `SELECT id FROM <table>` で最大 id を求め、次に書き込む行 id を返す
/// （空テーブルなら 0 から。`crash_tool_cross_table.rs::find_resume_state` と
/// 同じ「再起動のたびに既存状態から採番を続ける」設計）。
fn find_next_row_id(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Result<u64, String> {
    let result = core
        .execute_sql(ctx, &format!("SELECT id FROM {table} LIMIT {SCAN_LIMIT}"))
        .map_err(|e| format!("resume scan failed: {e}"))?;
    check_scan_not_truncated(result.rows.len())?;
    let max_id = result.rows.iter().map(|row| row.id).max();
    match max_id {
        Some(id) => id
            .checked_add(1)
            .ok_or_else(|| "row id overflow while resuming".to_string()),
        None => Ok(0),
    }
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
    ensure_table(&core, &ctx)?;
    // 2 表は独立に採番・再開する（kill が 2 表の INSERT の間に入っても各表の 0 起点連続性と
    // BATCH の倍数性は崩れない）。
    let mut next_row_ids = [0u64; TABLES.len()];
    for (slot, table) in next_row_ids.iter_mut().zip(TABLES) {
        *slot = find_next_row_id(&core, &ctx, table)?;
        if !slot.is_multiple_of(BATCH) {
            return Err(format!(
                "resumed row id {slot} of {table} is not a multiple of BATCH ({BATCH}); \
                 a previous run must have left a partial batch, which the atomic \
                 per-statement commit contract should make impossible"
            ));
        }
    }

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    for batch_no in 0..MAX_BATCHES {
        let _ = batch_no;
        let mut progress = (0u64, 0u64);
        for (index, (table, slot)) in TABLES.iter().zip(next_row_ids.iter_mut()).enumerate() {
            // バッチ通番は「これまでにコミットした行数 / BATCH」からそのまま導出する
            // （専用の台帳テーブルを持たないため、行 id の採番自体が単調増加かつ BATCH 単位
            // でしか進まないことを利用する）。
            let batch_seq = *slot / BATCH;
            let mut values = String::new();
            for offset in 0..BATCH {
                let id = slot
                    .checked_add(offset)
                    .ok_or_else(|| "row id overflow while writing".to_string())?;
                if offset > 0 {
                    values.push(',');
                }
                values.push_str(&row_tuple(table, id));
            }
            let columns = if *table == XIDX_TABLE || *table == PIDX_TABLE {
                "id, n, code"
            } else {
                "id, code"
            };
            let sql = format!(
                "INSERT INTO {table} ({columns}) VALUES {values} USING OPERATION_ID 'op-{table}-{batch_seq}'"
            );
            core.execute_insert_sql(&ctx, &sql)
                .map_err(|e| format!("insert failed at {table} batch_seq={batch_seq}: {e}"))?;
            if *table == PIDX_TABLE {
                // 先頭行の `n` の符号を反転する（-3〜3 の範囲を出ない）。述語・式要素の入退場を
                // kill の対象経路に乗せる。バッチ通番ごとに一意な操作 ID で、再開時に再実行されない。
                let flip = format!(
                    "UPDATE {table} SET n = 0 - n WHERE id = {slot} USING OPERATION_ID 'op-{table}-flip-{batch_seq}'"
                );
                let mut session = SessionState::default();
                core.execute_sql_in_session_legacy(&ctx, &mut session, &flip)
                    .map_err(|e| format!("update failed at {table} batch_seq={batch_seq}: {e}"))?;
            }
            *slot = slot
                .checked_add(BATCH)
                .ok_or_else(|| "row id overflow after insert".to_string())?;
            if index == 0 {
                progress = (batch_seq, *slot);
            }
        }

        // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する同期点
        // なので、バッファリングで遅延しないよう毎回明示的に flush する
        // （`crash_tool_cross_table.rs` と同方針）。
        writeln!(handle, "COMMITTED batch={} rows={}", progress.0, progress.1)
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;

        // 意図的なスロットリング: 本ツールは埋め込みベクトルを持たない軽量な行
        // だけを書くため、`crash_tool_cross_table.rs` と異なりコミット速度が
        // 非常に速く（実測で 1 秒あたり数万〜十数万行）、SIGKILL までの短い
        // 待機時間（`scripts/crash_test_unique_index.sh` の 10〜300ms）だけで
        // すぐ [`SCAN_LIMIT`]（SQL `SELECT` の `LIMIT` 上限 10000）を超えてしまう。
        // 1 バッチごとに短い sleep を挟み、SIGKILL までに積み上がる行数を
        // 抑える（クラッシュ耐性の検証対象は「commit 境界の原子性」であり、
        // 書き込みスループットではないため、この遅延は検証目的を損なわない）。
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

fn run_verify(path: &str) -> i32 {
    match verify_inner(path) {
        Ok((rows, batches)) => {
            println!("RESULT ok=true rows={rows} batches={batches}");
            0
        }
        Err(reason) => {
            println!("RESULT ok=false reason={reason}");
            1
        }
    }
}

fn verify_inner(path: &str) -> Result<(u64, u64), String> {
    let core = open_core(path)?;
    let ctx = tenant_ctx()?;
    // 両表に同じオラクルを適用する。報告する行数・バッチ数は `docs` のもの。
    let mut summary = (0, 0);
    for (index, table) in TABLES.iter().enumerate() {
        let totals =
            verify_table(&core, &ctx, table).map_err(|reason| format!("{table}: {reason}"))?;
        if index == 0 {
            summary = totals;
        }
    }
    Ok(summary)
}

/// 1 表分の検証本体（行 id の連続性・BATCH 整合・読み取り専用の索引検証・重複プローブ）。
fn verify_table(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Result<(u64, u64), String> {
    let result = core
        .execute_sql(ctx, &format!("SELECT id FROM {table} LIMIT {SCAN_LIMIT}"))
        .map_err(|e| format!("verify scan failed: {e}"))?;
    check_scan_not_truncated(result.rows.len())?;
    let mut ids: Vec<u64> = result.rows.iter().map(|row| row.id).collect();
    ids.sort_unstable();

    // 行 id の 0 起点連続性（`crash_tool_cross_table.rs` の行テーブル検証と
    // 同じオラクル。id は本ツールが単調に採番するため、欠落・重複はいずれも
    // クラッシュ耐性の破綻を示す）。
    for (expected, actual) in ids.iter().enumerate() {
        let expected = expected as u64;
        if *actual != expected {
            return Err(format!(
                "id gap or disorder: expected={expected} actual={actual}"
            ));
        }
    }
    let total_rows = ids.len() as u64;

    // 空虚な成功（vacuous pass）を拒否する: DB が空/未作成のまま検証をすり抜けない。
    if total_rows == 0 {
        return Err("no rows found".to_string());
    }
    // バッチ整合: 行総数は BATCH の倍数でなければならない（部分バッチが存在しない。
    // `write` 側が 1 INSERT 文＝1 write トランザクションで BATCH 件をまとめて
    // コミットするため、途中で kill されても部分バッチは残らないはずである）。
    if !total_rows.is_multiple_of(BATCH) {
        return Err(format!(
            "row count {total_rows} is not a multiple of BATCH ({BATCH}), partial batch suspected"
        ));
    }

    if table == XIDX_TABLE {
        verify_expression_index_table(core, ctx, total_rows)?;
        return Ok((total_rows, total_rows / BATCH));
    }
    if table == PIDX_TABLE {
        verify_partial_index_table(core, ctx, total_rows)?;
        return Ok((total_rows, total_rows / BATCH));
    }

    // 永続一意索引の不変条件そのものを検証する（本ツール固有のオラクル）:
    // 生存する各行の `code` 値は正引きエントリを持たなければならない
    // （`docs/design/unique-index.md`「正しさの不変条件」）。
    //
    // 読み取り専用の事前検証（PR #1123 レビュー対応。Codex 指摘: 索引
    // テーブル・マーカーが欠落していても、下の重複 INSERT プローブ自体が
    // 書き込み経路の `ensure_tenant_index` を経由して索引を静かに再構築して
    // しまうため、プローブは常に `23505` で拒否され欠落を見逃す）。プローブより
    // 前に、マーカーと各行の正引きエントリを読み取るだけで検証し、書き込みは
    // 一切行わない `verify_unique_index_read_only` を通す——ここで失敗すれば、
    // クラッシュ後の索引復旧が壊れていることを、再構築に隠蔽されずに検出できる。
    core.verify_unique_index_read_only(ctx, table)
        .map_err(|e| {
            format!("read-only unique index verification failed before duplicate probes: {e}")
        })?;
    // 既存 id の総数と衝突しない新しい id（`total_rows + id`）で同じ `code` 値の
    // 再挿入を試み、必ず `23505`（UNIQUE 制約違反）で拒否されることを確認する
    // （上記の読み取り専用検証を通過済みの索引に対する、書き込み経路からの
    // 二重確認）。拒否された文は副作用を残さない（fail-closed。行・索引の
    // いずれも書き換えられない）ため、`write` が再開する id 採番と衝突しない。
    for &id in &ids {
        let probe_id = total_rows
            .checked_add(id)
            .ok_or_else(|| "probe id overflow during verify".to_string())?;
        let probe_sql =
            format!("INSERT INTO {table} (id, code) VALUES ({probe_id}, 'code-{id}') USING OPERATION_ID 'verify-dup-{probe_id}'");
        match core.execute_insert_sql(ctx, &probe_sql) {
            Err(e) if e.wire_code() == "23505" => {}
            Err(e) => {
                return Err(format!(
                    "duplicate probe for existing code of id={id} failed with an unexpected \
                     wire_code {} (expected 23505): {e}",
                    e.wire_code()
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "duplicate probe for existing code of id={id} was wrongly accepted; \
                     the unique index is missing a forward entry after crash recovery"
                ));
            }
        }
    }

    let total_batches = total_rows / BATCH;
    Ok((total_rows, total_batches))
}

/// 式索引の表の検証（Issue #1649・INDEX-10）。再起動後も式索引の宣言と DML ゲートが生きており、
/// コミット済みの全行が式を評価できることを、公開 API だけで確かめる。いずれのプローブも常に
/// 拒否される前提のため、成功時は副作用を残さない（`write` が再開する id 採番と衝突しない）。
fn verify_expression_index_table(
    core: &EngineCore,
    ctx: &PolicyContext,
    total_rows: u64,
) -> Result<(), String> {
    // (0) 一意の式索引（Issue #1651）の永続一意索引が、再構築に隠蔽されずに生存している。
    core.verify_unique_index_read_only(ctx, XIDX_TABLE)
        .map_err(|e| {
            format!("read-only unique expression index verification failed before probes: {e}")
        })?;
    // (0b) 既存 code の大文字小文字違い（式 `lower(code)` が同値）の再挿入は必ず `23505`。
    for id in 0..total_rows {
        let probe_id = total_rows
            .checked_add(id)
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| "probe id overflow during verify".to_string())?;
        let sql = format!(
            "INSERT INTO {XIDX_TABLE} (id, n, code) VALUES ({probe_id}, 1, 'CODE-{id}') \
             USING OPERATION_ID 'verify-xidx-dup-{probe_id}'"
        );
        match core.execute_insert_sql(ctx, &sql) {
            Err(e) if e.wire_code() == "23505" => {}
            Err(e) => {
                return Err(format!(
                    "unique expression probe for id={id} failed with an unexpected wire_code {} \
                     (expected 23505): {e}",
                    e.wire_code()
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "unique expression probe for id={id} was wrongly accepted; the unique \
                     expression index lost a forward entry after crash recovery"
                ));
            }
        }
    }
    // (a) `n = 0` の INSERT は `22012` で拒否され、その id の行が残らない。
    let probe_id = total_rows;
    let insert_sql = format!(
        "INSERT INTO {XIDX_TABLE} (id, n, code) VALUES ({probe_id}, 0, 'probe') \
         USING OPERATION_ID 'verify-xidx-ins-{probe_id}'"
    );
    match core.execute_insert_sql(ctx, &insert_sql) {
        Err(e) if e.wire_code() == "22012" => {}
        Err(e) => {
            return Err(format!(
                "expression probe insert failed with an unexpected wire_code {} \
                 (expected 22012): {e}",
                e.wire_code()
            ));
        }
        Ok(_) => {
            return Err(
                "expression probe insert was wrongly accepted; the expression index gate \
                 is missing after crash recovery"
                    .to_string(),
            );
        }
    }
    // (b) 既存行を `n = 0` へ更新する UPDATE も `22012` で拒否される。
    let mut session = SessionState::default();
    let update_sql = format!(
        "UPDATE {XIDX_TABLE} SET n = 0 WHERE id = 0 USING OPERATION_ID 'verify-xidx-upd-{total_rows}'"
    );
    match core.execute_sql_in_session_legacy(ctx, &mut session, &update_sql) {
        Err(e) if e.wire_code() == "22012" => {}
        Err(e) => {
            return Err(format!(
                "expression probe update failed with an unexpected wire_code {} \
                 (expected 22012): {e}",
                e.wire_code()
            ));
        }
        Ok(_) => {
            return Err("expression probe update was wrongly accepted".to_string());
        }
    }
    // (c) 失敗値の行がコミット済みの中に 1 行も無い（派生索引を作り直せる）。
    let zero = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {XIDX_TABLE} WHERE n = 0 LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("verify zero scan failed: {e}"))?;
    if !zero.rows.is_empty() {
        return Err(format!(
            "{} committed row(s) have n = 0, which the expression index cannot evaluate",
            zero.rows.len()
        ));
    }
    // (d) 拒否された INSERT の行が残っていない（副作用ゼロ）。
    let probe = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {XIDX_TABLE} WHERE id = {probe_id} LIMIT 1"),
        )
        .map_err(|e| format!("verify probe scan failed: {e}"))?;
    if !probe.rows.is_empty() {
        return Err("rejected probe insert left a row behind".to_string());
    }
    Ok(())
}

/// 部分索引の表の検証（Issue #1654・INDEX-11）。再起動後も部分索引の宣言と DML ゲートが生きており、
/// コミット済みの全行で述語が評価でき、述語外（退場側）の行が永続化されていることを、公開 API だけで
/// 確かめる。いずれのプローブも常に拒否される前提のため、成功時は副作用を残さない。
fn verify_partial_index_table(
    core: &EngineCore,
    ctx: &PolicyContext,
    total_rows: u64,
) -> Result<(), String> {
    // (0) 部分 UNIQUE 索引（Issue #1657）の永続一意索引を、書き込みを一切しない読み取り専用の検証で
    //     確かめる。以降のプローブは書き込み経路の `ensure_tenant_index` で索引を再構築しうるため、
    //     必ず最初に行う。
    core.verify_unique_index_read_only(ctx, PIDX_TABLE)
        .map_err(|e| {
            format!("read-only partial unique index verification failed before probes: {e}")
        })?;
    // (0b) 述語内の既存行の code を新しい id・`n > 0` で再挿入すると `23505` で拒否される
    //      （部分 UNIQUE の正引きエントリがクラッシュ後も生きている）。
    let inside = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {PIDX_TABLE} WHERE n > 0 LIMIT 1"),
        )
        .map_err(|e| format!("verify inside-predicate scan failed: {e}"))?;
    if let Some(row) = inside.rows.first() {
        // `write` は code を `code-{id}` で書く（[`row_tuple`]）。
        let code = format!("code-{}", row.id);
        let dup_id = total_rows.saturating_add(1);
        let sql = format!(
            "INSERT INTO {PIDX_TABLE} (id, n, code) VALUES ({dup_id}, 1, '{code}') \
             USING OPERATION_ID 'verify-pidx-uniq-{dup_id}'"
        );
        match core.execute_insert_sql(ctx, &sql) {
            Err(e) if e.wire_code() == "23505" => {}
            Err(e) => {
                return Err(format!(
                    "partial unique probe failed with an unexpected wire_code {} \
                     (expected 23505): {e}",
                    e.wire_code()
                ));
            }
            Ok(_) => {
                return Err(
                    "partial unique probe insert was wrongly accepted; the partial unique \
                     index is not enforced after crash recovery"
                        .to_string(),
                );
            }
        }
    }
    // (a) 述語の評価エラー（`n = -5`）の INSERT は `22012` で拒否され、その id の行が残らない。
    let probe_id = total_rows;
    let insert_sql = format!(
        "INSERT INTO {PIDX_TABLE} (id, n, code) VALUES ({probe_id}, -5, 'probe') \
         USING OPERATION_ID 'verify-pidx-ins-{probe_id}'"
    );
    match core.execute_insert_sql(ctx, &insert_sql) {
        Err(e) if e.wire_code() == "22012" => {}
        Err(e) => {
            return Err(format!(
                "partial index probe insert failed with an unexpected wire_code {} \
                 (expected 22012): {e}",
                e.wire_code()
            ));
        }
        Ok(_) => {
            return Err(
                "partial index probe insert was wrongly accepted; the partial index gate \
                 is missing after crash recovery"
                    .to_string(),
            );
        }
    }
    // (b) 既存行を `n = -5` へ更新する UPDATE も `22012` で拒否される。
    let mut session = SessionState::default();
    let update_sql = format!(
        "UPDATE {PIDX_TABLE} SET n = -5 WHERE id = 0 USING OPERATION_ID 'verify-pidx-upd-{total_rows}'"
    );
    match core.execute_sql_in_session_legacy(ctx, &mut session, &update_sql) {
        Err(e) if e.wire_code() == "22012" => {}
        Err(e) => {
            return Err(format!(
                "partial index probe update failed with an unexpected wire_code {} \
                 (expected 22012): {e}",
                e.wire_code()
            ));
        }
        Ok(_) => {
            return Err("partial index probe update was wrongly accepted".to_string());
        }
    }
    // (c) 述語を評価できない行（`n = -5`）がコミット済みの中に 1 行も無い。
    let failing = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {PIDX_TABLE} WHERE n = -5 LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("verify failing-row scan failed: {e}"))?;
    if !failing.rows.is_empty() {
        return Err(format!(
            "{} committed row(s) have n = -5, which the partial index predicate cannot evaluate",
            failing.rows.len()
        ));
    }
    // (d) 述語外（退場側）の `n = 0` の行が永続化されている（`n = (id % 7) - 3` は id = 3 で 0。
    //     式要素 `100 / n` は述語 `n > 0` の外なので評価されず、行は受理される）。
    let outside = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {PIDX_TABLE} WHERE n = 0 LIMIT {SCAN_LIMIT}"),
        )
        .map_err(|e| format!("verify outside-predicate scan failed: {e}"))?;
    if outside.rows.is_empty() {
        return Err("no committed row outside the partial index predicate (n = 0)".to_string());
    }
    // (e) 拒否された INSERT の行が残っていない（副作用ゼロ）。
    let probe = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {PIDX_TABLE} WHERE id = {probe_id} LIMIT 1"),
        )
        .map_err(|e| format!("verify probe scan failed: {e}"))?;
    if !probe.rows.is_empty() {
        return Err("rejected probe insert left a row behind".to_string());
    }
    Ok(())
}
