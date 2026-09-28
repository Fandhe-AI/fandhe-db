//! RECOVER-8（`fail_fast`）の例外として、診断用 stderr 出力の書き込み失敗を
//! 無視するヘルパ（Issue #1081、ポインタ: `docs/spec/04-behavior/recovery.md`
//! RECOVER-8）。
//!
//! 背景（呼び出し元契約）: `eprintln!` は書き込み失敗（読み手が閉じた後の
//! `EPIPE` 等）で panic する。その panic は [`crate::recovery::panic_hook`]・
//! [`crate::recovery::fail_fast`] のフックを経て `std::process::abort()` まで
//! 進んでしまい、ログの受け手（パイプ・ログ収集プロセス）が閉じただけで
//! データ整合性と無関係にプロセス全体が落ちる（詳細は
//! `docs/design/stderr-log-write-failure.md` の ADR を参照）。
//!
//! 本モジュールが提供する [`write_line`]・[`crate::log_stderr`] は、この
//! 「診断ログの書き込み失敗」だけを無視する。範囲はこれに限り、
//! `fail_fast` 本体（panic は経路・スレッドを問わず abort する契約）・
//! `commit_boundary` の abort 条件（RECOVER-5）はいずれも変えない。panic は
//! 一切捕捉しない。
//!
//! 出力してはならないもの: テナント ID・行データ等の機密情報を stderr へ
//! 出さないことは、これまでどおり呼び出し側（wire-server の各ログ出力・
//! engine の実行時ログ）の責任である。本モジュールは書き込みの成否のみを
//! 扱い、出力内容の妥当性には関与しない。

use std::io::Write as _;

/// 診断用 stderr への 1 行書き込み。書き込みが失敗しても（`EPIPE` に限らず
/// `io::ErrorKind` を問わず）プロセスを継続し、エラーは握りつぶす。
///
/// [`crate::log_stderr`] マクロから呼ばれる想定で、直接呼ぶ場合も
/// `format_args!` の組み立てはすべて呼び出し側の固定書式リテラルに限る
/// （untrusted 入力を書式文字列として扱う経路は作らない）。
///
/// stderr のロックを 1 行分だけ保持する（`eprintln!` と同様。他スレッドの
/// 出力と行が混ざらないようにするが、ロック保持は最小限に留める）。
pub fn write_line(args: std::fmt::Arguments<'_>) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{args}");
}

/// `eprintln!` と同じ書式引数を受け付ける、書き込み失敗を無視する診断ログ
/// マクロ（RECOVER-8 の例外）。`#[macro_export]` により crate root に置かれる
/// ため、wire-server からは `engine::log_stderr!(...)` として呼ぶ
/// （`$crate` 経由で解決するため `package =` 名変更の影響を受けない）。
///
/// 置き換え対象: 実行時経路の診断ログ全般（wire-server の接続ハンドリング・
/// engine の実行時ログ）。置き換えないもの: `panic!`（本マクロは panic を
/// 発生させないため、fail-fast の abort 判断には影響しない）。
#[macro_export]
macro_rules! log_stderr {
    ($($arg:tt)*) => {
        $crate::recovery::stderr_log::write_line(::core::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::process::Stdio;

    const CHILD_MODE_ENV: &str = "ENGINE_STDERR_LOG_CHILD_MODE";
    const CHILD_SIGNAL_PATH_ENV: &str = "ENGINE_STDERR_LOG_CHILD_SIGNAL_PATH";

    /// 子プロセス側のエントリ。`CHILD_MODE_ENV` の値で分岐する:
    /// - `"lossy"`: `fail_fast::install()` の後、`log_stderr!` を複数回書いて
    ///   から正常終了する（読み手が閉じていても abort しないことの核心）。
    /// - `"eprintln_contrast"`: 同条件で `eprintln!` を 1 回書く（対照実験。
    ///   読み手が閉じた状態を正しく作れていることを SIGABRT で確認する）。
    ///
    /// いずれのモードも、親が読み口（`ChildStderr`）を drop した後に子が
    /// 書き込むよう、シグナルファイルの出現を待ってから書く
    /// （読み口を閉じる前に書いてしまう競合を防ぐ）。
    fn run_child_if_requested() {
        let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
            return;
        };
        let Ok(signal_path) = std::env::var(CHILD_SIGNAL_PATH_ENV) else {
            std::process::exit(2);
        };

        crate::recovery::fail_fast::install();

        // 親が stderr の読み口を drop するまで待つ（ポーリング。上限あり）。
        let start = std::time::Instant::now();
        while !std::path::Path::new(&signal_path).exists() {
            if start.elapsed() > std::time::Duration::from_secs(10) {
                std::process::exit(3);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 読み口の close がパイプへ反映されるまで僅かに待つ（即書き込みだと
        // EPIPE が未発生のまま成功してしまう flaky を避ける）。
        std::thread::sleep(std::time::Duration::from_millis(100));

        match mode.as_str() {
            "lossy" => {
                for i in 0..5 {
                    crate::log_stderr!("engine: stderr_log lossy test line {i}");
                }
                println!("CHILD_LOSSY_OK");
                std::process::exit(0);
            }
            "eprintln_contrast" => {
                eprintln!("engine: stderr_log eprintln contrast test line");
                println!("CHILD_REACHED_AFTER_EPRINTLN");
                std::process::exit(1);
            }
            other => {
                eprintln!("unknown {CHILD_MODE_ENV} value: {other}");
                std::process::exit(2);
            }
        }
    }

    #[test]
    fn run_child_dispatch() {
        run_child_if_requested();
    }

    /// 親プロセス側の共通ヘルパー: 自テストバイナリを `--exact` で再実行し、
    /// 子の stderr を `Stdio::piped()` にした直後に読み口を drop する
    /// （`fail_fast.rs::spawn_child` とは異なり、ここでは stderr を破棄せず
    /// 意図的に読み口を閉じることが検証の核心のため、専用の実装を持つ）。
    fn spawn_child_with_closed_stderr(mode: &str) -> (std::process::ExitStatus, String) {
        let exe = std::env::current_exe().expect("current_exe");
        let signal_path = std::env::temp_dir().join(format!(
            "engine-stderr-log-signal-{}-{}-{}",
            std::process::id(),
            mode,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_file(&signal_path);

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--exact")
            .arg("recovery::stderr_log::tests::run_child_dispatch")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_MODE_ENV, mode)
            .env(
                CHILD_SIGNAL_PATH_ENV,
                signal_path.to_str().expect("signal path is valid utf-8"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn child process");

        // 読み口を drop して「読み手が閉じた」状態を作る（本テストの核心）。
        drop(child.stderr.take());

        // 読み口を閉じた後で子へ書き込み許可のシグナルを送る。
        std::fs::write(&signal_path, b"go").expect("write signal file");

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&signal_path);
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        let mut stdout_buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout_buf);
        }
        let _ = std::fs::remove_file(&signal_path);
        (status, stdout_buf)
    }

    #[cfg(unix)]
    fn assert_sigabrt(status: std::process::ExitStatus, stdout_buf: &str) {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            status.signal(),
            Some(6),
            "child must be terminated by SIGABRT (std::process::abort); \
             status={status:?} stdout={stdout_buf}"
        );
    }

    // --- 核心: 読み手が閉じた後の `log_stderr!` 書き込み失敗はプロセスを
    // 継続させること（Issue #1081 の受入基準）。

    #[test]
    fn log_stderr_write_failure_does_not_abort() {
        let (status, stdout_buf) = spawn_child_with_closed_stderr("lossy");
        assert!(
            status.success(),
            "log_stderr! must tolerate a closed stderr reader and exit successfully; \
             status={status:?} stdout={stdout_buf}"
        );
        assert!(
            stdout_buf.contains("CHILD_LOSSY_OK"),
            "child must reach the success marker after 5 lossy writes; stdout={stdout_buf}"
        );
    }

    // --- 対照実験: 同条件で `eprintln!` を使うと abort すること
    // （「読み手が閉じた」状態を正しく作れていることの確認 ―― 空振り防止）。

    #[test]
    fn eprintln_write_failure_still_aborts_as_contrast() {
        let (status, stdout_buf) = spawn_child_with_closed_stderr("eprintln_contrast");
        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_EPRINTLN"),
            "eprintln! must abort before reaching the line after it; stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child must not exit successfully; status={status:?} stdout={stdout_buf}"
        );
        #[cfg(unix)]
        assert_sigabrt(status, &stdout_buf);
    }
}
