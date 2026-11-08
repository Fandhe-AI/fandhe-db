//! 同期レプリケーション（オプション。Issue #2245・親 #2171・RECOVER-15 ポインタ）の確認待ちを、
//! テナント向けの応答境界から呼ぶ入口。方式の記録は `docs/design/replication-streaming.md` の
//! 「実装判断（Issue #2245）」。
//!
//! 責務境界: engine は commit のたびに、その接続スレッドが commit した最大の LSN を
//! `engine::recovery::commit_boundary::take_pending_sync_lsn` で渡す（変更ログ有効時だけ。書き込み
//! ゲートの permit は engine から戻った時点で解放済み）。本モジュールは、その LSN を
//! `EngineCore::sync_commit()` の調停役（`engine::storage::SyncCommitCoordinator`）で待ち、確認が取れる前に
//! 成功を返さないようにする。待機が上限を超える（`reject`）・中断された場合は、呼び出し元が commit 済みの
//! 応答を `state=may_be_committed`（ERR-5）のエラーへ差し替える。engine の commit の `Err` 契約
//! （durable でない）は変えない。
//!
//! 呼び出し場所は `ResponseBoundaryGuard::new()` のある 4 か所（`simple_query`・`extended_query`・`copy`・
//! `http/conn`）で、engine の呼び出しが戻った後・応答を 1 バイトも書く前に呼ぶ。呼び忘れは構造テスト
//! （本ファイルの `every_response_boundary_awaits_sync_commit`）で固定する。
//!
//! 同期の水準は CLI だけで決まり、テナントのセッション・SQL・HTTP からは変えられない。エラー文言は固定の
//! 英語で、ロール名・LSN・遅れ・テナント情報を含めない（RECOVER-15 (8)）。

use engine::core::EngineCore;
use engine::recovery::commit_boundary::take_pending_sync_lsn;
use engine::storage::SyncWaitOutcome;

/// 確認が取れなかった場合のエラー文言（固定の英語。`D` の `state=may_be_committed` と併せて返す）。
pub const NOT_CONFIRMED_MESSAGE: &str =
    "synchronous replication was not confirmed; the transaction may have been committed";

/// 同期レプリケーションの確認が取れなかった（上限超過・中断）。commit はプライマリで確定済み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotConfirmed;

/// この接続スレッドが応答前に commit した LSN の確認を待つ。同期が無効、または commit が無ければ即
/// `Ok`（非同期の既定経路では Cell の読み取りと `None` 判定だけ）。`interrupted` は約 100ms ごとに確認する。
pub fn await_sync_commit(
    core: &EngineCore,
    interrupted: impl Fn() -> bool,
) -> Result<(), NotConfirmed> {
    // 同期が無効でも必ず消費する（古い値が次の応答へ残らないようにする）。
    let Some(lsn) = take_pending_sync_lsn() else {
        return Ok(());
    };
    let Some(coordinator) = core.sync_commit() else {
        return Ok(());
    };
    match coordinator.wait(lsn, interrupted) {
        SyncWaitOutcome::Confirmed | SyncWaitOutcome::Degraded => Ok(()),
        SyncWaitOutcome::TimedOut | SyncWaitOutcome::Interrupted => Err(NotConfirmed),
    }
}

/// pg wire の `ErrorResponse`（`D` = `state=may_be_committed`）を書く。接続は維持し、`ReadyForQuery` は
/// 呼び出し元が書く（簡易クエリのエラー応答と同じ分担）。
pub fn write_not_confirmed<S: crate::wire_stream::WireStream>(
    stream: &mut S,
) -> std::io::Result<()> {
    let frame = crate::error_response::encode_with_detail(
        engine::error_format::ErrorClass::InternalError,
        NOT_CONFIRMED_MESSAGE,
        crate::error_response::MAY_BE_COMMITTED_DETAIL,
    )
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "failed to encode ErrorResponse",
        )
    })?;
    stream.write_all(&frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `ResponseBoundaryGuard::new()` で応答境界を張るファイルは、必ず `await_sync_commit(` も呼ぶ
    /// （呼び忘れると、同期レプリケーション有効時に確認前の成功を返してしまう。RECOVER-15）。
    #[test]
    fn every_response_boundary_awaits_sync_commit() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let mut boundaries = 0usize;
        for f in files {
            let text = std::fs::read_to_string(&f).expect("read");
            // 本ファイル自身（検査文字列を含む）と定義側は除く。
            if f.ends_with("sync_commit_wait.rs") {
                continue;
            }
            let prod = &text;
            if prod.contains("ResponseBoundaryGuard::new()") {
                boundaries += 1;
                assert!(
                    prod.contains("await_sync_commit("),
                    "{} creates a ResponseBoundaryGuard but never awaits sync commit",
                    f.display()
                );
            }
        }
        assert_eq!(boundaries, 4, "response boundary sites changed; review");
    }

    #[test]
    fn without_coordinator_the_wait_is_a_no_op() {
        let _ = take_pending_sync_lsn();
        let db = std::env::temp_dir().join(format!("fandhe-sync-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&db);
        std::fs::create_dir_all(&db).expect("mkdir");
        let core = EngineCore::open(db.join("t.redb")).expect("open");
        assert_eq!(await_sync_commit(&core, || false), Ok(()));
        let _ = std::fs::remove_dir_all(&db);
    }
}
