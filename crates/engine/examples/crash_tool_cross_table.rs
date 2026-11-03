//! TASK-90（対象ビヘイビア: TABLE-10。ポインタ: `docs/spec/05-tasks.md` TASK-90）の
//! 2 テーブル横断トランザクション・クラッシュ耐性回帰テスト用ツール。
//!
//! `engine::txn::BatchWriteTxn`（[`ROWS_TABLE`] + `BATCH_LOG_TABLE` を同一
//! トランザクションで扱う `log_batch` を持つ TASK-90 専用の型）の公開 API のみを
//! 使うブラックボックスバイナリで、
//! `scripts/crash_test_cross_table.sh` から `write` サブコマンドをバックグラウンド起動され
//! SIGKILL される想定、続けて `verify` サブコマンドで再オープン後の内容整合性を
//! 検証される想定で作られている。単一テーブル・単一行コミットを検証する
//! `crash_tool.rs`（TASK-142・PERSIST-1。本ブランチの時点では未マージにつき、
//! マージ後に write/verify の出力プロトコル様式を追随予定）と役割は近いが、本ツールは
//! 「行テーブルとバッチ台帳という 2 テーブルが常に運命を共にする」ことをオラクルとする点で
//! 異なる（単一テーブルの単一行コミットではなく、複数行 + バッチ台帳 1 エントリを
//! 1 トランザクションでコミットし続ける）。
//!
//! サブコマンド:
//! - `write <db_path>`: `BATCH` 件の行 put + `log_batch` を 1 トランザクションで
//!   コミットし続ける。`COMMITTED batch=<seq> rows=<total>` を stdout へ出力する
//!   （`scripts/crash_test_cross_table.sh` の開始同期点）。再起動時は `scan_page`（行 ID）・
//!   `batch_log_max_seq`（バッチ通番。Issue #132 で `scan_batch_log` 全件走査から
//!   移行。台帳件数の上限に依存せず再開できる）から採番を再開する。
//! - `verify <db_path>`: 行 ID の 0 起点連続性・内容一致（行）、`batch_seq` の
//!   0 起点連続性（台帳）、さらに「台帳の row_count 合計 == 行総数」（テーブル間整合）と
//!   「行総数が BATCH の倍数」（バッチ整合）を検証する。結果を
//!   `RESULT ok=true rows=<N> batches=<M>` / `RESULT ok=false reason=<...>` の 1 行で
//!   stdout へ出力し、失敗時は非 0 終了する（fail-closed。空虚な成功を許さない）。
//!
//! 明示トランザクションモード（Issue #2415。親 TASK-332。ビヘイビア: SQL-31・TABLE-10）:
//! 上記は storage API 直叩きの 1 トランザクションを検証するが、利用者が実際に使う
//! 「SQL の `BEGIN` … `COMMIT` に複数文を積んで 2 表以上を更新する」経路は対象外だった。
//! `write <db_path> txn` / `verify <db_path> txn` / `self-check <workdir> txn` は SQL 表層
//! （`EngineCore::execute_sql_in_txn`）だけを使い、条件付き `UPDATE`（台帳表）と `INSERT`
//! （明細表・バッチ表）を組にした 3 表横断の明示トランザクションを繰り返し、再オープン後に
//! 「全部残るか全く残らないか」を検査する。writer は `BEGIN` ではなく最初の書き込み文で取得
//! される前提（Issue #2396）に依存しないよう、Tx 内で `SELECT` して条件を決めず、期待値は
//! `UPDATE` の `WHERE` に畳み込む。どの文でも `55P03`・`40001` 等が起こりうる前提で扱う。
//! DDL の kill 制御は #2417／#2418 の担当。
//!
//! kill 位置の制御（Issue #2416。ビヘイビア: PERSIST-1・SQL-31・TABLE-10）:
//! - `write <db_path> txn` は台帳 `UPDATE` の直後に `IN_TXN batch=<k>` を出す。スクリプトは
//!   最後の行が `IN_TXN` なら「Tx の途中に kill が当たった」と分類し、全反復で 1 度も観測
//!   しなければ失敗にする（空虚な成功の拒否）。
//! - `write-pause <db_path> <in-txn|before-commit|after-commit> <warmup>` は `warmup` 件を
//!   コミットしたのち、次の Tx の指定点で `PAUSED point=<p> batch=<k> expect_rows=<n>
//!   expect_batches=<m>` を出して停止する（`PAUSE_LIMIT_SECS` で上限。SIGKILL が来なければ
//!   非 0 終了）。`scripts/crash_test_txn_kill_points.sh` が PAUSED を待って kill -9 し、
//!   再起動後の verify が期待値と完全一致することを確かめる。

use std::io::Write as _;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, StorageError, Visibility};

// 従来方式（暗黙の `id` 疑似列を持つ）の表を SQL から作るテスト専用ヘルパー（TABLE-23）。
// txn モードの表作成に使う（他の crash ツールと同じ取り込み方式）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;

/// write/verify で固定して使うテナント識別子（単一テナントのクラッシュ耐性検証が
/// 目的で、RLS ポリシー評価そのものは対象外。TASK-142・PERSIST-1 のクラッシュ耐性
/// ツールと同方針だが、当該ツールは本ブランチの時点では未マージ）。
const CRASH_TOOL_TENANT_ID: &str = "crash-tool-cross-table-tenant";

/// 1 トランザクションで書き込む行数（バッチサイズ）。行総数はこの値の倍数になる
/// （verify のバッチ整合オラクル）。
const BATCH: u64 = 10;
/// 生成する埋め込みベクトルの次元数。
const EMBEDDING_DIM: usize = 64;
/// 生成するメタデータバイト列の長さ。
const METADATA_LEN: usize = 32;
/// `scan_page` の 1 回あたり取得件数。
const PAGE_LIMIT: u32 = 10_000;
/// write の自走上限バッチ数（安全弁）。`scripts/crash_test_cross_table.sh` は書き込み
/// 進行中に SIGKILL する想定のため通常はここへ到達しない。到達した場合は無限ループに
/// せず正常終了する（security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_BATCHES: u64 = 500_000;

/// 実行モード。既定の `Batch` は storage API 直叩き、`Txn` は SQL 明示トランザクション
/// （Issue #2415）。後続 Issue が同じ形でモードを足せるよう enum にしている。
#[derive(Clone, Copy)]
enum Mode {
    Batch,
    Txn,
}

fn parse_mode(s: &str) -> Option<Mode> {
    match s {
        "batch" => Some(Mode::Batch),
        "txn" => Some(Mode::Txn),
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let exit_code = match refs.as_slice() {
        ["write", path] => run_write(path),
        ["verify", path] => run_verify(path),
        ["write", path, mode] => match parse_mode(mode) {
            Some(Mode::Batch) => run_write(path),
            Some(Mode::Txn) => run_write_txn(path),
            None => usage(),
        },
        ["verify", path, mode] => match parse_mode(mode) {
            Some(Mode::Batch) => run_verify(path),
            Some(Mode::Txn) => run_verify_txn(path),
            None => usage(),
        },
        ["write-pause", path, point, warmup] => run_write_pause(path, point, warmup),
        ["self-check", workdir, "txn"] => run_self_check_txn(workdir),
        _ => usage(),
    };
    std::process::exit(exit_code);
}

fn usage() -> i32 {
    eprintln!(
        "usage: crash_tool_cross_table <write|verify> <db_path> [batch|txn] | \
         write-pause <db_path> <in-txn|before-commit|after-commit> <warmup> | \
         self-check <workdir> txn"
    );
    2
}
/// 決定的な擬似乱数生成器（splitmix64。追加依存を増やさない方針 - dependency-policy.md。
/// TASK-142・PERSIST-1 のクラッシュ耐性ツールと同一実装を意図しているが、当該ツールは
/// 本ブランチの時点では未マージにつきマージ後に整合を確認予定）。
struct DeterministicRng(u64);

impl DeterministicRng {
    fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// 行 ID から埋め込み・メタデータを決定的に導出する（write が生成した内容を verify が
/// 同じ関数で再計算し突き合わせる。TASK-142・PERSIST-1 の `derive_row` 相当と同方針だが、
/// 当該実装は本ブランチの時点では未マージ）。
fn derive_row(id: u64) -> (Vec<f32>, Vec<u8>) {
    let mut rng = DeterministicRng::seeded(id);
    let embedding = (0..EMBEDDING_DIM)
        .map(|_| {
            let word = rng.next_u64();
            ((word % 200_001) as f32 - 100_000.0) / 1000.0
        })
        .collect();
    let metadata = (0..METADATA_LEN)
        .map(|_| (rng.next_u64() % 256) as u8)
        .collect();
    (embedding, metadata)
}

/// 既存データから採番を再開するための状態（行 ID の続き・バッチ通番の続き）。
struct ResumeState {
    next_row_id: u64,
    next_batch_seq: u64,
}

/// `scan_page`（行テーブル）・`batch_log_max_seq`（バッチ台帳）の両方から採番を再開する
/// （クラッシュ → 再起動 → 再クラッシュを反復するために必要）。バッチ台帳側は
/// `scan_batch_log` の全件走査ではなく `batch_log_max_seq`（redb の B-tree 末尾キー
/// 取得のみ・O(log n)）を使うため、台帳件数が `MAX_BATCH_LOG_ROWS` を超えても
/// 再開経路自体は上限に依存しない（Issue #132）。
fn find_resume_state(storage: &Storage) -> Result<ResumeState, String> {
    let mut cursor: Option<engine::storage::RowCursor> = None;
    // 本ツールは単一テナント（`CRASH_TOOL_TENANT_ID`）のみを書き込むため、物理キー
    // `(tenant_id, id)`（TABLE-12）の走査順はそのまま `id` 昇順になる。複数テナントを
    // 書き込む場合は「最後の行」ではなく走査した全行の `id` の `max()` を取る必要がある。
    let mut max_row_id: Option<u64> = None;
    loop {
        let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (rows, next_cursor) = storage
            .scan_page(cursor_ref, PAGE_LIMIT)
            .map_err(|e| format!("scan_page failed: {e}"))?;
        for row in &rows {
            max_row_id = Some(max_row_id.map_or(row.id, |m| m.max(row.id)));
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let next_row_id = match max_row_id {
        Some(id) => id
            .checked_add(1)
            .ok_or_else(|| "row id overflow while resuming".to_string())?,
        None => 0,
    };

    let next_batch_seq = match storage
        .batch_log_max_seq()
        .map_err(|e| format!("batch_log_max_seq failed: {e}"))?
    {
        Some(max_seq) => max_seq
            .checked_add(1)
            .ok_or_else(|| "batch seq overflow while resuming".to_string())?,
        None => 0,
    };

    Ok(ResumeState {
        next_row_id,
        next_batch_seq,
    })
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
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    let resume = find_resume_state(&storage)?;
    let mut next_row_id = resume.next_row_id;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();

    for batch_no in 0..MAX_BATCHES {
        // 既存台帳の最大通番が u64::MAX 付近の場合、無検査の加算だと debug では panic、
        // release ではラップアラウンドして既存の batch_seq=0 を再利用してしまう
        // （`log_batch` は既存 batch_seq への上書きを DuplicateBatchSeq で拒否するが、
        // 折り返し自体は checked_add で事前に検出し fail-closed に終了する方が
        // 早期に・分かりやすいエラーで失敗できる）。
        let batch_seq = resume
            .next_batch_seq
            .checked_add(batch_no)
            .ok_or_else(|| "batch seq overflow while writing".to_string())?;
        // TABLE-10 の 2 テーブル横断コミットそのものを検証する経路。同一トランザクション
        // 内で行テーブル（BATCH 件）とバッチ台帳（1 エントリ）の両方へ書き込む。
        let mut txn = storage
            .begin_batch_write()
            .map_err(|e| format!("begin_batch_write failed: {e}"))?;
        for _ in 0..BATCH {
            let (embedding, metadata) = derive_row(next_row_id);
            let row = RowInput {
                tenant_id: CRASH_TOOL_TENANT_ID,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: &metadata,
            };
            txn.put(next_row_id, &row)
                .map_err(|e| format!("put failed: {e}"))?;
            next_row_id = next_row_id
                .checked_add(1)
                .ok_or_else(|| "row id overflow after put".to_string())?;
        }
        txn.log_batch(batch_seq)
            .map_err(|e| format!("log_batch failed: {e}"))?;
        txn.commit().map_err(|e| format!("commit failed: {e}"))?;

        // 進捗行はスクリプト側が「書き込みが実際に進み始めた」ことを検知する同期点
        // なので、バッファリングで遅延しないよう毎回明示的に flush する。
        writeln!(handle, "COMMITTED batch={batch_seq} rows={next_row_id}")
            .map_err(|e| format!("stdout write failed: {e}"))?;
        handle
            .flush()
            .map_err(|e| format!("stdout flush failed: {e}"))?;
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
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;

    // 行テーブル側: ID の 0 起点連続性・内容一致（PERSIST-1 のオラクルと同方針）。
    let mut cursor: Option<engine::storage::RowCursor> = None;
    let mut expected_id: u64 = 0;
    let mut total_rows: u64 = 0;
    loop {
        let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (rows, next_cursor) = storage
            .scan_page(cursor_ref, PAGE_LIMIT)
            .map_err(|e| format!("scan_page failed: {e}"))?;
        for row in &rows {
            if row.id != expected_id {
                return Err(format!(
                    "id gap or disorder: expected={expected_id} actual={}",
                    row.id
                ));
            }
            if row.tenant_id != CRASH_TOOL_TENANT_ID {
                return Err(format!(
                    "tenant_id mismatch at id={}: expected={CRASH_TOOL_TENANT_ID} actual={}",
                    row.id, row.tenant_id
                ));
            }
            if row.visibility != Visibility::Public {
                return Err(format!(
                    "visibility mismatch at id={}: expected=Public actual={:?}",
                    row.id, row.visibility
                ));
            }
            let (expected_embedding, expected_metadata) = derive_row(row.id);
            if row.embedding != expected_embedding {
                return Err(format!("embedding mismatch at id={}", row.id));
            }
            if row.metadata != expected_metadata {
                return Err(format!("metadata mismatch at id={}", row.id));
            }
            expected_id = expected_id
                .checked_add(1)
                .ok_or_else(|| "row id overflow during verify".to_string())?;
            total_rows = total_rows
                .checked_add(1)
                .ok_or_else(|| "row count overflow during verify".to_string())?;
        }
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    // バッチ台帳側: batch_seq の 0 起点連続性・row_count 合計。
    //
    // `StorageError::ScanLimitExceeded` の `Display` はそのまま使わない（Issue #131・
    // PR #193 codex レビュー再指摘対応）: 固定文言 `"scan limit exceeded: use scan_page"`
    // は `Storage::scan` 専用の代替 API 案内であり、台帳（`scan_batch_log`。ページング API
    // を持たない）には当てはまらない。加えて台帳エントリ数を削減する compact/rotate 相当の
    // API・運用手順も本リポには存在しない（通常の compaction は論理エントリ数を減らさず、
    // rotation で台帳を捨てれば検証対象そのものを失うため、いずれも実行可能な代替手段では
    // ない: PR #193 codex レビュー再指摘対応）。実行不能な手段を示唆せず、「このツールでは
    // 上限を超えた台帳を検証できない」という事実のみを明示する
    // （`storage.rs::Storage::scan_batch_log` のドキュメンテーションコメント参照）。
    let mut batch_log = storage.scan_batch_log().map_err(|e| match e {
        StorageError::ScanLimitExceeded => {
            "scan_batch_log failed: batch log exceeds the scan limit of this tool; \
             cross-table verification cannot be performed on this ledger \
             (no paginated ledger API is available yet)"
                .to_string()
        }
        other => format!("scan_batch_log failed: {other}"),
    })?;
    batch_log.sort_by_key(|(seq, _)| *seq);
    let mut expected_seq: u64 = 0;
    let mut total_from_log: u64 = 0;
    for (seq, row_count) in &batch_log {
        if *seq != expected_seq {
            return Err(format!(
                "batch seq gap or disorder: expected={expected_seq} actual={seq}"
            ));
        }
        if *row_count != BATCH {
            return Err(format!(
                "batch row_count mismatch at seq={seq}: expected={BATCH} actual={row_count}"
            ));
        }
        total_from_log = total_from_log
            .checked_add(*row_count)
            .ok_or_else(|| "batch row_count sum overflow during verify".to_string())?;
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or_else(|| "batch seq overflow during verify".to_string())?;
    }
    let total_batches = batch_log.len() as u64;

    // 空虚な成功（vacuous pass）を拒否する: DB が空/未作成のまま検証をすり抜けない。
    if total_rows == 0 || total_batches == 0 {
        return Err("no rows or no batches found".to_string());
    }
    // テーブル間整合（TABLE-10 の中核オラクル）: バッチ台帳の row_count 合計と
    // 行テーブルの総行数が一致すること。片方だけコミットされて他方が失われていない
    // （＝2 テーブル横断コミットが原子的だった）ことの直接的な確認。
    if total_from_log != total_rows {
        return Err(format!(
            "cross-table mismatch: batch_log total={total_from_log} row_count={total_rows}"
        ));
    }
    // バッチ整合: 行総数は BATCH の倍数でなければならない（部分バッチが存在しない）。
    if !total_rows.is_multiple_of(BATCH) {
        return Err(format!(
            "row count {total_rows} is not a multiple of BATCH ({BATCH}), partial batch suspected"
        ));
    }

    Ok((total_rows, total_batches))
}
// ======================================================================
// txn モード（Issue #2415）: SQL 明示トランザクションで 3 表をまたぐ複数文
// ======================================================================

type Res<T> = Result<T, String>;

/// txn モードで固定して使うテナント。
const TXN_TENANT: &str = "crash-tool-xtxn-tenant";
/// 別テナント（verify が境界の非漏えいを軽く確かめる）。
const TXN_OTHER_TENANT: &str = "crash-tool-xtxn-other";
/// 1 Tx で `xt_items` へ入れる行数（2 文に分けて入れる）。
const TXN_BATCH: u64 = 4;
/// write の自走上限 Tx 数（安全弁。`SELECT … LIMIT` の上限未満に保つ）。
const MAX_TXNS: u64 = 2_000;
/// `SELECT … LIMIT` の上限。到達したら全件を見られないので fail-closed。
const SCAN_LIMIT: u64 = 10_000;
/// 文間の待機。Tx 実行中に kill が当たる確率を上げ、行数の増加速度も抑える。
const STMT_PAUSE_MS: u64 = 3;

fn txn_ctx(tenant: &str) -> Res<PolicyContext> {
    // SQL 表層の INSERT は可視性を Private で書くため、両方を許可する。
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .map_err(|e| format!("invalid tenant context: {e}"))
}

fn open_txn_core(path: &str) -> Res<EngineCore> {
    let storage = Storage::open(path).map_err(|e| format!("open failed: {e}"))?;
    Ok(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ))
}

fn sql_head(sql: &str) -> String {
    sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// 1 文を autocommit で実行する。エラー文言は `wire_code` と文頭のみ（行データを含めない）。
fn txn_exec(core: &EngineCore, tenant: &str, sql: &str) -> Res<SqlOutcome> {
    let c = txn_ctx(tenant)?;
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&c, &mut s, sql)
        .map_err(|e| format!("{} failed ({})", sql_head(sql), e.wire_code()))
}

/// 失敗時は ROLLBACK を試みて閉じる明示 Tx 実行器。
struct TxnSession<'a> {
    core: &'a EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: engine::sql::transaction::SessionTransaction<'a>,
}

impl<'a> TxnSession<'a> {
    fn begin(core: &'a EngineCore, tenant: &str) -> Res<Self> {
        let ctx = txn_ctx(tenant)?;
        let mut session = SessionState::default();
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

    /// 文を実行する。失敗したら ROLLBACK を試みてから `Err` を返す
    /// （`55P03`・`40001` を含め、どの文の失敗も同一に扱う）。
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

fn payload_of(seq: u64, j: u64) -> String {
    format!("p-{seq}-{j}")
}

/// 3 表を（無ければ）作り、台帳の単一行を補う。冪等。
fn ensure_txn_schema(core: &EngineCore) -> Res<()> {
    for ddl in [
        "CREATE TABLE xt_ledger (applied BIGINT)",
        "CREATE TABLE xt_items (seq BIGINT, payload TEXT)",
        "CREATE TABLE xt_batches (seq BIGINT, n BIGINT)",
    ] {
        match txn_exec(core, TXN_TENANT, ddl) {
            Ok(_) => {}
            // 既存表（42P07）は再起動時の正常系。
            Err(e) if e.contains("(42P07)") => {}
            Err(e) => return Err(e),
        }
    }
    if select_rows(core, TXN_TENANT, "SELECT id, applied FROM xt_ledger")?.is_empty() {
        txn_exec(
            core,
            TXN_TENANT,
            "INSERT INTO xt_ledger (id, applied) VALUES (0, 0)",
        )?;
    }
    Ok(())
}

/// `(id, cells)` を id 昇順に全件読む。上限到達は fail-closed。
fn select_rows(core: &EngineCore, tenant: &str, sql_body: &str) -> Res<Vec<(u64, Vec<Cell>)>> {
    let sql = format!("{sql_body} ORDER BY id LIMIT {SCAN_LIMIT}");
    match txn_exec(core, tenant, &sql)? {
        SqlOutcome::Query(q) => {
            if q.rows.len() as u64 >= SCAN_LIMIT {
                return Err("scan limit reached; cannot see all rows".to_string());
            }
            Ok(q.rows.into_iter().map(|r| (r.id, r.cells)).collect())
        }
        _ => Err("expected a query outcome".to_string()),
    }
}

/// 行の cells から BIGINT 列（`SignedInteger`）だけを順に拾う（`id` は `Integer` なので混ざらない）。
fn bigints_of(cells: &[Cell]) -> Vec<i64> {
    cells
        .iter()
        .filter_map(|c| match c {
            Cell::SignedInteger(n) => Some(*n),
            _ => None,
        })
        .collect()
}

/// 3 表の内容。`items`: `(id, seq, payload)`、`batches`: `(id, seq, n)`。
struct TxnSnapshot {
    ledger: Vec<(u64, i64)>,
    items: Vec<(u64, i64, String)>,
    batches: Vec<(u64, i64, i64)>,
}

fn read_snapshot(core: &EngineCore, tenant: &str) -> Res<TxnSnapshot> {
    let ledger = select_rows(core, tenant, "SELECT id, applied FROM xt_ledger")?
        .into_iter()
        .map(|(id, cells)| match bigints_of(&cells).first() {
            Some(a) => Ok((id, *a)),
            None => Err("unexpected ledger row".to_string()),
        })
        .collect::<Res<Vec<_>>>()?;
    let items = select_rows(core, tenant, "SELECT id, seq, payload FROM xt_items")?
        .into_iter()
        .map(|(id, cells)| {
            let seq = bigints_of(&cells).first().copied();
            let payload = cells.iter().find_map(|c| match c {
                Cell::Text(t) => Some(t.clone()),
                _ => None,
            });
            match (seq, payload) {
                (Some(s), Some(p)) => Ok((id, s, p)),
                _ => Err("unexpected items row".to_string()),
            }
        })
        .collect::<Res<Vec<_>>>()?;
    let batches = select_rows(core, tenant, "SELECT id, seq, n FROM xt_batches")?
        .into_iter()
        .map(|(id, cells)| {
            let v = bigints_of(&cells);
            match (v.first(), v.get(1)) {
                (Some(s), Some(n)) => Ok((id, *s, *n)),
                _ => Err("unexpected batches row".to_string()),
            }
        })
        .collect::<Res<Vec<_>>>()?;
    Ok(TxnSnapshot {
        ledger,
        items,
        batches,
    })
}

/// 3 表の整合を検査し、コミット済み Tx 数 `S` を返す。部分コミット・欠落・重複は `Err`。
/// `allow_empty` が偽なら `S == 0`（空虚な成功）も `Err`。
fn check_snapshot(snap: &TxnSnapshot, allow_empty: bool) -> Res<u64> {
    let applied = match snap.ledger.as_slice() {
        [(0, a)] => u64::try_from(*a).map_err(|_| "negative applied".to_string())?,
        _ => {
            return Err(format!(
                "ledger rows={} (expected one id=0 row)",
                snap.ledger.len()
            ))
        }
    };
    let item_total = snap.items.len() as u64;
    let batch_total = snap.batches.len() as u64;
    let expect_items = applied
        .checked_mul(TXN_BATCH)
        .ok_or_else(|| "item count overflow".to_string())?;
    if batch_total != applied {
        return Err(format!(
            "cross-table mismatch: ledger applied={applied} batches={batch_total}"
        ));
    }
    if item_total != expect_items {
        return Err(format!(
            "cross-table mismatch: ledger applied={applied} items={item_total} \
             (expected {expect_items})"
        ));
    }
    for (i, (id, seq, n)) in snap.batches.iter().enumerate() {
        let i = i as u64;
        if *id != i || u64::try_from(*seq).ok() != Some(i) {
            return Err(format!("batch seq gap or disorder at index {i}"));
        }
        if u64::try_from(*n).ok() != Some(TXN_BATCH) {
            return Err(format!("batch n mismatch at seq={i}"));
        }
    }
    for (i, (id, seq, payload)) in snap.items.iter().enumerate() {
        let i = i as u64;
        let want_seq = i / TXN_BATCH;
        if *id != i {
            return Err(format!("item id gap or disorder at index {i}"));
        }
        if u64::try_from(*seq).ok() != Some(want_seq) {
            return Err(format!("item seq mismatch at id={i}"));
        }
        if *payload != payload_of(want_seq, i % TXN_BATCH) {
            return Err(format!("item payload mismatch at id={i}"));
        }
    }
    if !allow_empty && applied == 0 {
        return Err("no committed transactions found".to_string());
    }
    Ok(applied)
}

fn run_write_txn(path: &str) -> i32 {
    match write_txn_inner(path) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

/// 明示 Tx の中で SIGKILL を当てたい位置（Issue #2416）。`run_one_txn` が各位置を通過した
/// 直後に観測フックへ通知する。ランダム kill 用（`write`）と決定的 kill 用（`write-pause`）が
/// 同じ Tx 本体を共有し、両者の文列が食い違わないようにする。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TxnPoint {
    /// 台帳 `UPDATE` の成功直後（writer 取得済みで、未コミットの書き込みがある）。
    AfterFirstWrite,
    /// 明細前半の `INSERT` の成功直後（2 表に未コミットの書き込みが途中まである）。
    InTxn,
    /// バッチ `INSERT` の成功直後で、`COMMIT` を送る前。
    BeforeCommit,
    /// `COMMIT` が `Ok` を返した直後で、ack 行を出す前。
    AfterCommit,
}

/// 3 表をまたぐ明示 Tx を 1 回分実行する。`obs` は各 [`TxnPoint`] の通過直後に呼ばれる。
/// `write_txn_inner`（ランダム kill）と `write_pause_inner`（決定的 kill）から呼ばれる。
fn run_one_txn(
    core: &EngineCore,
    k: u64,
    obs: &mut dyn FnMut(TxnPoint, u64) -> Res<()>,
) -> Res<()> {
    let pause = || std::thread::sleep(std::time::Duration::from_millis(STMT_PAUSE_MS));
    let mut t = TxnSession::begin(core, TXN_TENANT)?;
    pause();
    // writer は最初の書き込み文で取得される。期待値は WHERE に畳み込み、Tx 内で SELECT しない。
    match t.run(&format!(
        "UPDATE xt_ledger SET applied = applied + 1 WHERE id = 0 AND applied = {k}"
    ))? {
        SqlOutcome::Update(u) if u.rows_affected == 1 => {}
        _ => {
            let _ = t.run("ROLLBACK");
            return Err("ledger update did not match exactly one row".to_string());
        }
    }
    obs(TxnPoint::AfterFirstWrite, k)?;
    pause();
    let half = TXN_BATCH / 2;
    for (idx, range) in [0..half, half..TXN_BATCH].into_iter().enumerate() {
        let values: Vec<String> = range
            .map(|j| {
                let id = k * TXN_BATCH + j;
                format!("({id}, {k}, '{}')", payload_of(k, j))
            })
            .collect();
        t.run(&format!(
            "INSERT INTO xt_items (id, seq, payload) VALUES {}",
            values.join(", ")
        ))?;
        if idx == 0 {
            obs(TxnPoint::InTxn, k)?;
        }
        pause();
    }
    t.run(&format!(
        "INSERT INTO xt_batches (id, seq, n) VALUES ({k}, {k}, {TXN_BATCH})"
    ))?;
    obs(TxnPoint::BeforeCommit, k)?;
    pause();
    t.run("COMMIT")?;
    obs(TxnPoint::AfterCommit, k)
}

/// stdout へ 1 行書き切って flush する（kill 時に行が途中で切れる窓を最小化する）。
fn emit_line(line: &str) -> Res<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{line}").map_err(|e| format!("stdout write failed: {e}"))?;
    out.flush().map_err(|e| format!("stdout flush failed: {e}"))
}

/// 3 表をまたぐ明示 Tx を繰り返す。台帳 `UPDATE` 直後に `IN_TXN batch=<k>` を出し
/// （Tx の途中に kill が当たったことをスクリプトが観測する印）、`COMMIT` 成功後にだけ
/// `COMMITTED` を出す。
fn write_txn_inner(path: &str) -> Res<()> {
    let core = open_txn_core(path)?;
    ensure_txn_schema(&core)?;
    // 再開: 3 表が整合していなければ fail-closed で開始を拒否する。
    let start = check_snapshot(&read_snapshot(&core, TXN_TENANT)?, true)
        .map_err(|e| format!("inconsistent state on resume: {e}"))?;

    for n in 0..MAX_TXNS {
        let k = start
            .checked_add(n)
            .ok_or_else(|| "txn seq overflow".to_string())?;
        run_one_txn(&core, k, &mut |point, k| match point {
            TxnPoint::AfterFirstWrite => emit_line(&format!("IN_TXN batch={k}")),
            TxnPoint::AfterCommit => {
                let total = k
                    .checked_add(1)
                    .and_then(|v| v.checked_mul(TXN_BATCH))
                    .ok_or_else(|| "row count overflow".to_string())?;
                emit_line(&format!("COMMITTED batch={k} rows={total}"))
            }
            _ => Ok(()),
        })?;
    }
    Ok(())
}

/// `write-pause` の一時停止の上限秒数。SIGKILL が来なければ非 0 終了する（無限待ちしない）。
const PAUSE_LIMIT_SECS: u64 = 60;

fn parse_point(s: &str) -> Option<TxnPoint> {
    match s {
        "in-txn" => Some(TxnPoint::InTxn),
        "before-commit" => Some(TxnPoint::BeforeCommit),
        "after-commit" => Some(TxnPoint::AfterCommit),
        _ => None,
    }
}

fn run_write_pause(path: &str, point: &str, warmup: &str) -> i32 {
    let (Some(point), Ok(warmup)) = (parse_point(point), warmup.parse::<u64>()) else {
        return usage();
    };
    if warmup == 0 || warmup > MAX_TXNS {
        return usage();
    }
    match write_pause_inner(path, point, warmup) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("ERROR: {reason}");
            1
        }
    }
}

/// `warmup` 件の完全な Tx をコミットしたのち、次の Tx の指定点で `PAUSED` を出して停止する。
/// 期待値は途中・COMMIT 直前では「その Tx は全く残らない」、COMMIT 直後では「全部残る」
/// （COMMIT は redb の Immediate commit を同期的に行うため）。
fn write_pause_inner(path: &str, point: TxnPoint, warmup: u64) -> Res<()> {
    let core = open_txn_core(path)?;
    ensure_txn_schema(&core)?;
    let start = check_snapshot(&read_snapshot(&core, TXN_TENANT)?, true)
        .map_err(|e| format!("inconsistent state on resume: {e}"))?;
    let target = start
        .checked_add(warmup)
        .ok_or_else(|| "txn seq overflow".to_string())?;

    for k in start..target {
        run_one_txn(&core, k, &mut |p, k| {
            if p != TxnPoint::AfterCommit {
                return Ok(());
            }
            let total = k
                .checked_add(1)
                .and_then(|v| v.checked_mul(TXN_BATCH))
                .ok_or_else(|| "row count overflow".to_string())?;
            emit_line(&format!("COMMITTED batch={k} rows={total}"))
        })?;
    }

    let (name, committed_batches) = match point {
        TxnPoint::AfterCommit => ("after-commit", target.checked_add(1)),
        TxnPoint::BeforeCommit => ("before-commit", Some(target)),
        _ => ("in-txn", Some(target)),
    };
    let expect_batches = committed_batches.ok_or_else(|| "txn seq overflow".to_string())?;
    let expect_rows = expect_batches
        .checked_mul(TXN_BATCH)
        .ok_or_else(|| "row count overflow".to_string())?;
    run_one_txn(&core, target, &mut |p, k| {
        if p != point {
            return Ok(());
        }
        emit_line(&format!(
            "PAUSED point={name} batch={k} expect_rows={expect_rows} expect_batches={expect_batches}"
        ))?;
        std::thread::sleep(std::time::Duration::from_secs(PAUSE_LIMIT_SECS));
        Err("pause limit reached without SIGKILL".to_string())
    })
}

fn run_verify_txn(path: &str) -> i32 {
    match verify_txn_inner(path) {
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

fn verify_txn_inner(path: &str) -> Res<(u64, u64)> {
    let core = open_txn_core(path)?;
    let applied = check_snapshot(&read_snapshot(&core, TXN_TENANT)?, false)?;
    // 別テナントからは 3 表とも 0 行に見えること（テナント境界の軽い確認）。
    for t in ["xt_ledger", "xt_items", "xt_batches"] {
        let rows = select_rows(&core, TXN_OTHER_TENANT, &format!("SELECT id FROM {t}"))?;
        if !rows.is_empty() {
            return Err("tenant boundary violated: foreign rows visible".to_string());
        }
    }
    let rows = applied
        .checked_mul(TXN_BATCH)
        .ok_or_else(|| "row count overflow".to_string())?;
    Ok((rows, applied))
}

/// オラクルの空振り防止。正常な DB は ok、3 表の一部だけが進んだ DB は ng になることを確かめる。
fn run_self_check_txn(workdir: &str) -> i32 {
    match self_check_txn(workdir) {
        Ok(()) => {
            println!("SELF-CHECK ok");
            0
        }
        Err(reason) => {
            eprintln!("ERROR: self-check failed: {reason}");
            1
        }
    }
}

/// 1 Tx 分のうち指定した表（台帳・明細・バッチ）だけを autocommit で書く（部分コミットの再現用）。
fn write_partial(core: &EngineCore, k: u64, ledger: bool, items: bool, batch: bool) -> Res<()> {
    if ledger {
        txn_exec(
            core,
            TXN_TENANT,
            &format!("UPDATE xt_ledger SET applied = applied + 1 WHERE id = 0 AND applied = {k}"),
        )?;
    }
    if items {
        let values: Vec<String> = (0..TXN_BATCH)
            .map(|j| format!("({}, {k}, '{}')", k * TXN_BATCH + j, payload_of(k, j)))
            .collect();
        txn_exec(
            core,
            TXN_TENANT,
            &format!(
                "INSERT INTO xt_items (id, seq, payload) VALUES {}",
                values.join(", ")
            ),
        )?;
    }
    if batch {
        txn_exec(
            core,
            TXN_TENANT,
            &format!("INSERT INTO xt_batches (id, seq, n) VALUES ({k}, {k}, {TXN_BATCH})"),
        )?;
    }
    Ok(())
}

fn self_check_txn(workdir: &str) -> Res<()> {
    // (名前, 台帳, 明細, バッチ, 期待: verify が ok か)
    let cases: [(&str, bool, bool, bool, bool); 4] = [
        ("complete", true, true, true, true),
        ("items-only", false, true, false, false),
        ("ledger-only", true, false, false, false),
        ("batch-only", false, false, true, false),
    ];
    for (i, (name, l, it, b, want_ok)) in cases.iter().enumerate() {
        let db = format!("{workdir}/self_check_txn_{i}.redb");
        let core = open_txn_core(&db)?;
        ensure_txn_schema(&core)?;
        // 先頭に完全な 1 Tx を置き、その後に case の Tx（k=1）を積む。
        write_partial(&core, 0, true, true, true)?;
        write_partial(&core, 1, *l, *it, *b)?;
        drop(core);
        let got = verify_txn_inner(&db).is_ok();
        if got != *want_ok {
            return Err(format!("case {name}: verify ok={got}, expected {want_ok}"));
        }
    }
    // 空の DB（0 Tx）は空虚な成功として拒否されること。
    let db = format!("{workdir}/self_check_txn_empty.redb");
    let core = open_txn_core(&db)?;
    ensure_txn_schema(&core)?;
    drop(core);
    if verify_txn_inner(&db).is_ok() {
        return Err("empty database was accepted".to_string());
    }
    Ok(())
}
