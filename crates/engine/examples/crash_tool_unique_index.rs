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
const CRASH_TOOL_TENANT_ID: &str = "crash-tool-unique-index-tenant";

/// 1 `INSERT` 文（＝1 write トランザクション）で書き込む行数。行総数はこの値の
/// 倍数になる（verify のバッチ整合オラクル。`crash_tool_cross_table.rs` と同方針）。
const BATCH: u64 = 10;
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
/// `SqlSurfaceError::DuplicateTable` を無視して継続する）。
fn ensure_table(core: &EngineCore, ctx: &PolicyContext) -> Result<(), String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    match core.execute_sql_in_session(ctx, &mut session, "CREATE TABLE docs (code TEXT UNIQUE)") {
        Ok(_) => Ok(()),
        Err(engine::sql::allowlist::SqlSurfaceError::DuplicateTable { .. }) => Ok(()),
        Err(e) => Err(format!("create table failed: {e}")),
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

/// `SELECT id FROM docs` で最大 id を求め、次に書き込む行 id を返す
/// （空テーブルなら 0 から。`crash_tool_cross_table.rs::find_resume_state` と
/// 同じ「再起動のたびに既存状態から採番を続ける」設計）。
fn find_next_row_id(core: &EngineCore, ctx: &PolicyContext) -> Result<u64, String> {
    let result = core
        .execute_sql(ctx, &format!("SELECT id FROM docs LIMIT {SCAN_LIMIT}"))
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
    let mut next_row_id = find_next_row_id(&core, &ctx)?;
    if !next_row_id.is_multiple_of(BATCH) {
        return Err(format!(
            "resumed row id {next_row_id} is not a multiple of BATCH ({BATCH}); \
             a previous run must have left a partial batch, which the atomic \
             per-statement commit contract should make impossible"
        ));
    }

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    for batch_no in 0..MAX_BATCHES {
        // バッチ通番は「これまでにコミットした行数 / BATCH」からそのまま導出する
        // （`crash_tool_cross_table.rs` と異なり専用の台帳テーブルを持たないため、
        // 行 id の採番自体が単調増加かつ BATCH 単位でしか進まないことを利用する）。
        let batch_seq = next_row_id / BATCH;
        let _ = batch_no;

        let mut values = String::new();
        for offset in 0..BATCH {
            let id = next_row_id
                .checked_add(offset)
                .ok_or_else(|| "row id overflow while writing".to_string())?;
            if offset > 0 {
                values.push(',');
            }
            values.push_str(&format!("({id}, 'code-{id}')"));
        }
        let sql = format!(
            "INSERT INTO docs (id, code) VALUES {values} USING OPERATION_ID 'op-{batch_seq}'"
        );
        core.execute_insert_sql(&ctx, &sql)
            .map_err(|e| format!("insert failed at batch_seq={batch_seq}: {e}"))?;
        next_row_id = next_row_id
            .checked_add(BATCH)
            .ok_or_else(|| "row id overflow after insert".to_string())?;

        // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する同期点
        // なので、バッファリングで遅延しないよう毎回明示的に flush する
        // （`crash_tool_cross_table.rs` と同方針）。
        writeln!(handle, "COMMITTED batch={batch_seq} rows={next_row_id}")
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

    let result = core
        .execute_sql(&ctx, &format!("SELECT id FROM docs LIMIT {SCAN_LIMIT}"))
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
    core.verify_unique_index_read_only(&ctx, "docs")
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
            format!("INSERT INTO docs (id, code) VALUES ({probe_id}, 'code-{id}') USING OPERATION_ID 'verify-dup-{probe_id}'");
        match core.execute_insert_sql(&ctx, &probe_sql) {
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
