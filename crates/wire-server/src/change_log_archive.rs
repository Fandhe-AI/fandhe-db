//! 変更ログのアーカイブ常駐スレッド（Issue #2167・親 #2164・RECOVER-14 ポインタ。設計は
//! `docs/design/change-log-pitr.md` の D6）。
//!
//! 責務境界: `main.rs::run_server` が、`--change-log-archive-dir` の指定時に 1 本だけ起動する。起動直後に
//! 1 回、以降は設定された周期で `EngineCore::archive_change_log`（engine の `Storage::archive_change_log`）を
//! 呼ぶだけで、セグメントの書き出し・水位更新・刈り込みは engine が担う。クライアントのセッション
//! スレッドとは独立で、利用者の commit に相乗りしない（書き込みゲートは engine 側が通常の書き込みへ譲る）。
//!
//! 失敗は固定文言を stderr へ出して次の周期で再試行する（アーカイブされていないレコードは engine が決して
//! 削除しない）。同じ内容の失敗は連続して 1 回しか出さず、利用者の操作でログが溢れない。文言にパス・LSN・
//! 件数を含めない。未アーカイブの量が上限（Issue #3091）に達した・下回った状態遷移も、変化したときだけ
//! 固定文言で 1 行ずつ出す
//! （運用者向け。テナントのセッションには見せない）。
//! プロセス終了時に停止を待たない（書き出しは一時名 → 公開の順で、途中で止まっても
//! 次回の起動が収束させる）。

use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::storage::{ArchiveError, ArchiveOptions};

use crate::change_log_opt::ArchiveSettings;

/// 周期の途中でスレッドが占有しないよう、待機は短い刻みに分けない（`sleep` 1 回）。
fn options(settings: &ArchiveSettings) -> ArchiveOptions {
    let mut opts = ArchiveOptions::new(settings.dir.clone());
    opts.retain_records = settings.retain_records;
    opts
}

/// 見送り（書き込みゲートを譲り続けた）が連続したときに警告を出す回数。常時書き込みが続くと水位が進まず、
/// 未アーカイブのレコードが DB 内で増え続けるため、運用者へ知らせる。
pub const SKIPPED_WARN_STREAK: u32 = 6;

/// 周期をまたいで持つ状態（同じ内容の失敗を繰り返し出さない・見送りの連続回数）。
#[derive(Debug, Default)]
pub struct State {
    last_error: Option<String>,
    skipped_streak: u32,
    /// 前回の周期で未アーカイブ量が上限に達していたか（変化したときだけ通知する）。
    at_limit: bool,
}

/// 未アーカイブ量が上限に達した遷移の固定文言（件数・LSN・パスを含めない）。
pub const LIMIT_REACHED_MESSAGE: &str = "change log unarchived backlog reached its limit; writes are refused until the archive catches up";
/// 未アーカイブ量が上限を下回った遷移の固定文言。
pub const LIMIT_CLEARED_MESSAGE: &str =
    "change log unarchived backlog is below its limit again; writes are accepted";

/// 1 回分を実行し、stderr へ出すべき固定文言を返す。同じ失敗は連続して 1 回だけ、見送りの警告は
/// [`SKIPPED_WARN_STREAK`] 回連続した時点で 1 回だけ返す。成功すると記憶をクリアする。
/// アーカイブの成否にかかわらず、未アーカイブ量の上限の到達・解消が変わったときは遷移の文言を足す。
pub fn run_once(core: &EngineCore, opts: &ArchiveOptions, state: &mut State) -> Vec<String> {
    let mut out: Vec<String> = archive_cycle(core, opts, state).into_iter().collect();
    // 状態を読めない周期は遷移を判定しない（次の周期に回す）。
    if let Ok(Some(status)) = core.change_log_unarchived_status() {
        if status.at_limit != state.at_limit {
            state.at_limit = status.at_limit;
            out.push(
                if status.at_limit {
                    LIMIT_REACHED_MESSAGE
                } else {
                    LIMIT_CLEARED_MESSAGE
                }
                .to_string(),
            );
        }
    }
    out
}

fn archive_cycle(core: &EngineCore, opts: &ArchiveOptions, state: &mut State) -> Option<String> {
    match core.archive_change_log(opts) {
        Ok(report) => {
            state.last_error = None;
            if !report.skipped {
                state.skipped_streak = 0;
                return None;
            }
            state.skipped_streak = state.skipped_streak.saturating_add(1);
            (state.skipped_streak == SKIPPED_WARN_STREAK).then(|| {
                "change log archive is being skipped because writers keep the write gate busy; \
                 unarchived records are accumulating"
                    .to_string()
            })
        }
        Err(e) => {
            let text = error_text(&e);
            if state.last_error.as_deref() == Some(text.as_str()) {
                None
            } else {
                state.last_error = Some(text.clone());
                Some(text)
            }
        }
    }
}

/// 失敗の固定文言。パス・LSN・件数は含めず、I/O 失敗には原因の分類（`ErrorKind` の名前）だけを添える
/// （容量不足と権限不足を運用者が区別できるようにする）。
fn error_text(e: &ArchiveError) -> String {
    match e {
        ArchiveError::Io(kind) => format!("change log archive failed: {e} ({kind:?})"),
        _ => format!("change log archive failed: {e}"),
    }
}

/// 常駐スレッドを起動する。スレッドを作れなかった場合は `Err`（起動拒否にする）。
///
/// 各周期は `catch_unwind` で包み、panic しても固定文言を stderr へ出してスレッドを続ける。サーバー自体は
/// 止めない判断: アーカイブは利用者の読み書きの正しさに関与せず（未アーカイブの行は DB に残り、刈り込みも
/// 起きない）、止めると利用者の可用性を落とすだけで、停止は stderr で運用者に分かる。
pub fn spawn(core: Arc<EngineCore>, settings: &ArchiveSettings) -> std::io::Result<()> {
    let opts = options(settings);
    let interval: Duration = settings.interval;
    std::thread::Builder::new()
        .name("change-log-archive".into())
        .spawn(move || {
            let mut state = State::default();
            loop {
                let cycle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_once(&core, &opts, &mut state)
                }));
                match cycle {
                    Ok(msgs) => {
                        for msg in msgs {
                            engine::log_stderr!("wire-server: {msg}");
                        }
                    }
                    Err(_) => engine::log_stderr!(
                        "wire-server: change log archive cycle panicked; it will be retried"
                    ),
                }
                std::thread::sleep(interval);
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_messages_carry_no_variable_content() {
        for m in [LIMIT_REACHED_MESSAGE, LIMIT_CLEARED_MESSAGE] {
            assert!(!m.contains('{') && !m.chars().any(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn io_failures_carry_only_the_error_kind() {
        let t = error_text(&ArchiveError::Io(std::io::ErrorKind::PermissionDenied));
        assert!(t.contains("PermissionDenied"), "{t}");
        let t = error_text(&ArchiveError::LsnGap);
        assert!(!t.contains('(') || !t.contains("Kind"), "{t}");
    }
}
