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
//! 件数を含めない。プロセス終了時に停止を待たない（書き出しは一時名 → 公開の順で、途中で止まっても
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

/// 1 回分を実行し、直前の失敗文言と比べて出力すべき文言を返す（成功・見送りは `None` で、記憶をクリアする）。
pub fn run_once(
    core: &EngineCore,
    opts: &ArchiveOptions,
    last_error: &mut Option<String>,
) -> Option<String> {
    match core.archive_change_log(opts) {
        Ok(_) => {
            *last_error = None;
            None
        }
        Err(e) => {
            let text = error_text(&e);
            if last_error.as_deref() == Some(text.as_str()) {
                None
            } else {
                *last_error = Some(text.clone());
                Some(text)
            }
        }
    }
}

fn error_text(e: &ArchiveError) -> String {
    format!("change log archive failed: {e}")
}

/// 常駐スレッドを起動する。スレッドを作れなかった場合は `Err`（起動拒否にする）。
pub fn spawn(core: Arc<EngineCore>, settings: &ArchiveSettings) -> std::io::Result<()> {
    let opts = options(settings);
    let interval: Duration = settings.interval;
    std::thread::Builder::new()
        .name("change-log-archive".into())
        .spawn(move || {
            let mut last_error: Option<String> = None;
            loop {
                if let Some(msg) = run_once(&core, &opts, &mut last_error) {
                    engine::log_stderr!("wire-server: {msg}");
                }
                std::thread::sleep(interval);
            }
        })
        .map(|_| ())
}
