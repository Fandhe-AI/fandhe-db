//! TASK-99（対象ビヘイビア: RECOVER-8。ポインタ: `docs/spec/05-tasks.md` TASK-99・
//! `docs/spec/04-behavior/recovery.md` RECOVER-8）の実装。
//!
//! 契約: プロセス内で発生するエラーを 2 系統に統一する。
//!
//! 1. **回復可能エラー**（`Result::Err` として捕捉されるもの）: 既存の
//!    [`crate::error_format`]（TASK-152・ERR-2）が `wire_code` 契約のエラー応答へ
//!    写像し、プロセス・接続は処理を継続する。本モジュールはこちらには関与しない。
//! 2. **panic**: 読み取り経路・書き込み経路を問わず、発生箇所・スレッドに関わらず
//!    プロセスを即座に終了する（fail-fast）。プロセス内で共有される状態（redb の
//!    ハンドル・接続プール等）が panic により汚染された可能性がある以上、
//!    「当該スレッドのみを殺して他接続の処理を継続する」既定の Rust の挙動より、
//!    即時終了して外側の再起動機構に委ねる方が安全側という判断による。
//!
//! 責務境界（[`crate::recovery::commit_boundary`]・[`crate::recovery::panic_hook`]
//! との違い）: `commit_boundary` の各ガードは「commit 成功後の panic に限り」
//! abort する狭い安全弁（TASK-96・RECOVER-5）であり、`panic_hook` はその abort
//! 前段で緊急応答の送出を試みる観測可能性側（TASK-97・RECOVER-6）に過ぎない。
//! いずれも「commit-pending でない panic」（例: 読み取り経路中の panic、commit を
//! 一切伴わない処理中の panic）を捕捉しない。本モジュールは exhaustive な
//! fail-fast をプロセス全体へ一括で与える最終防衛線であり、[`install`] は既存の
//! `panic_hook::install_panic_hook` の**後**に呼ぶ契約（下記「導入順序」参照）。
//!
//! 導入順序（呼び出し元契約）: `wire-server::main::run_server` が起動時に
//! `panic_hook::install_panic_hook()` を呼んだ**直後**に本モジュールの
//! [`install`] を呼ぶ。`std::panic::set_hook` はプロセスに 1 つのフックしか
//! 保持できないため、[`install`] は「自分がフックへ差し替わる際に捕捉した
//! 直前のフック（＝ `panic_hook` が既に差し込んだもの）を必ず先に呼んでから
//! abort する」ことで、TASK-97・RECOVER-6 の緊急応答送出を退行させずに
//! fail-fast を最外層として被せる（`panic_hook` 側は「緊急応答を送れた場合は
//! 前フックを呼ばない」契約のため、緊急応答が送れなかった通常経路でのみ
//! `panic_hook` から `install` 前のフック〔Rust 既定など〕へ更に委譲される。
//! いずれの分岐でも [`install`] の abort は unwind の続きとして必ず実行される）。
//!
//! 呼び出し先の限定: engine 側のライブラリ初期化（`EngineCore::open` 等）からは
//! 呼ばない契約（[`crate::recovery::panic_hook::install_panic_hook`] と同じ理由
//! ―― engine 単体の
//! `catch_unwind` を使うテスト・engine をライブラリとして使う他バイナリの
//! panic 挙動を暗黙に変えないため）。`install()` を呼ぶのは
//! `wire-server::main::run_server` のみを想定する。
//!
//! 診断出力の書き込み失敗は本モジュールの対象外（Issue #1081、
//! ポインタ: `docs/design/stderr-log-write-failure.md`）: 本フックが出す固定
//! 文言も含め、stderr への診断ログ出力自体は
//! [`crate::recovery::stderr_log::write_line`]／[`crate::log_stderr`] を通し、
//! 読み手が閉じた後の書き込み失敗（`EPIPE` 等）を無視する。panic 自体を捕捉
//! しない点・panic が発生した場合に必ず abort する点は従来どおり変わらない。

use std::sync::Once;

/// fail-fast フックの冪等な導入（TASK-99・RECOVER-8）。複数回呼ばれても実際の
/// フック差し替えは 1 回のみ行う（`std::sync::Once`。`panic_hook` の
/// `install_panic_hook` とは別の `Once` インスタンスであり、互いの冪等性を
/// 混同しない）。
///
/// 呼び出し元契約はモジュールドキュメント「導入順序」を参照。
/// `wire-server::main::run_server` が `panic_hook::install_panic_hook()` の直後に
/// 呼ぶ想定。
pub fn install() {
    static INSTALL_ONCE: Once = Once::new();
    INSTALL_ONCE.call_once(|| {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // 直前のフック（`panic_hook::install_panic_hook` が差し込んだもの、
            // または未導入なら Rust 既定）を必ず先に呼ぶ。TASK-97・RECOVER-6 の
            // 緊急応答送出（送れた場合のみ前フックをスキップする内部契約）や
            // 既定フックの stderr 出力を、本フックの追加で失わせないため。
            previous_hook(panic_info);
            // untrusted 入力起因の panic メッセージをそのまま流用せず、固定文言の
            // みを stderr へ出す（panic_info の Display 実装は既に前フック側で
            // 出力済み・出力するかは前フックの判断に委ねる。ここでは fail-fast
            // が発動したことのみを明示する）。
            crate::log_stderr!(
                "wire-server: fatal: unrecovered panic, aborting process (fail-fast, RECOVER-8)"
            );
            // 経路（読み取り・書き込み）・発生スレッドを問わず、ここへ到達した
            // panic は必ずプロセスを終了させる。`commit_boundary` の狭い abort
            // 条件（armed && panicking）には依存しない ―― 本フックは無条件。
            std::process::abort();
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- install の冪等性: 複数回呼んでも panic しないこと ---
    // abort を伴わない検証のみ（実際に abort するかはサブプロセステストで担保）。
    // 本テストプロセス自身で `install()` を呼ぶと、以降このテストバイナリ内の
    // 他のあらゆる panic（他テストの `catch_unwind` を含む）が abort してしまう
    // ため、`install()` 呼び出し自体をサブプロセスへ隔離する（下記
    // `subprocess_*` テスト群と同型）。ここでは「関数呼び出しが panic しない」
    // ことだけをインプロセスで示す代わりに、サブプロセスの冪等性検証で兼ねる。

    const CHILD_MODE_ENV: &str = "ENGINE_FAIL_FAST_CHILD_MODE";
    const CHILD_MARKER_PATH_ENV: &str = "ENGINE_FAIL_FAST_CHILD_MARKER_PATH";

    /// 子プロセス側のエントリ。`CHILD_MODE_ENV` の値に応じて分岐する:
    /// - `"idempotent"`: `install()` を複数回呼んだ後、panic せず正常終了する
    ///   （固定マーカーを出力してから `exit(0)`）。
    /// - `"main_thread_panic"`: `install()` を 1 回呼んだ後、メインスレッドで
    ///   panic する（fail-fast により SIGABRT で終了するはず）。
    /// - `"worker_thread_panic"`: `install()` を 1 回呼んだ後、
    ///   `std::thread::spawn` したワーカースレッドで panic し、`join()` する
    ///   （既定の Rust 挙動なら当該スレッドのみが死んでプロセスは継続してしまう
    ///   ケース ―― RECOVER-8 が「経路を問わず」を満たすことの核心的な検証）。
    /// - `"hook_chain"`: `CHILD_MARKER_PATH_ENV` が指すパスへ副作用を書き込む
    ///   フック（TASK-97・RECOVER-6 の `panic_hook::install_panic_hook` が
    ///   差し込むものの代役 ―― 「直前のフック」の存在を単純化して表す）を
    ///   `install()` より先に `std::panic::set_hook` で仕込んでから `install()`
    ///   を呼び、panic する。`install()` が直前のフックを呼ばずに abort する
    ///   誤実装（TASK-97・RECOVER-6 の緊急応答退行）を、SIGABRT の確認だけでは
    ///   検出できないため、マーカーファイルへの書き込みで直接検証する
    ///   （advisor 指摘対応）。
    fn run_child_if_requested() {
        let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
            return;
        };

        match mode.as_str() {
            "idempotent" => {
                install();
                install();
                install();
                println!("CHILD_IDEMPOTENT_OK");
                std::process::exit(0);
            }
            "main_thread_panic" => {
                install();
                panic!("injected main-thread panic for RECOVER-8 verification");
            }
            "worker_thread_panic" => {
                install();
                let handle =
                    std::thread::spawn(|| panic!("injected worker-thread panic for RECOVER-8"));
                // 通常の Rust 挙動なら join がこの Err を受け取ってプロセスは
                // 継続する。fail-fast フックがワーカースレッドの panic 時点で
                // 既にプロセスを abort させている契約のため、ここへ到達すること
                // 自体が不具合。
                let _ = handle.join();
                println!("CHILD_REACHED_AFTER_WORKER_JOIN");
                std::process::exit(1);
            }
            "hook_chain" => {
                let Ok(marker_path) = std::env::var(CHILD_MARKER_PATH_ENV) else {
                    std::process::exit(2);
                };
                // `panic_hook::install_panic_hook()` が実運用で差し込む「直前の
                // フック」の役割を、副作用のみに単純化して代役させる（実際の
                // TASK-97・RECOVER-6 の緊急応答経路は wire-server 側の TCP 接続を
                // 前提とし、engine 単体のサブプロセステストへ持ち込むと accept/
                // connect の競合で flaky になりやすいため、ここでは「install()
                // が直前のフックを必ず呼んでから abort するか」という契約だけを
                // 直接検証する）。
                std::panic::set_hook(Box::new(move |_| {
                    let _ = std::fs::write(&marker_path, b"PREV_HOOK_CALLED");
                }));
                install();
                panic!("injected panic to verify hook chaining for RECOVER-8");
            }
            other => {
                eprintln!("unknown {CHILD_MODE_ENV} value: {other}");
                std::process::exit(2);
            }
        }
    }

    /// 親プロセス側の共通ヘルパー: 自テストバイナリ（`current_exe()`）を
    /// `--exact` フィルタで再実行し、指定した env を渡して子プロセス化する
    /// （`commit_boundary.rs` の `subprocess_post_commit_panic_aborts_before_
    /// returning_success` と同型のハーネス。stderr は `Stdio::null()` で
    /// 破棄し、panic フックの出力で OS のパイプバッファが埋まって子が
    /// `abort()` 前に write でブロックし親がタイムアウトする flaky を避ける）。
    fn spawn_child(
        mode: &str,
        extra_env: Option<(&str, &str)>,
    ) -> (std::process::ExitStatus, String) {
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--exact")
            .arg("recovery::fail_fast::tests::run_child_dispatch")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_MODE_ENV, mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some((k, v)) = extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn child process");

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        use std::io::Read as _;
        let mut stdout_buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout_buf);
        }
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

    /// 実際の子プロセスエントリはこの 1 関数に集約する（`--exact` で名指しする
    /// テスト関数名を固定するため。分岐は `CHILD_MODE_ENV` の値で行う）。
    /// 環境変数が未設定の通常の `cargo test` 実行では即座に return する
    /// no-op であり、通常の `cargo test -p fandhe-vector-db-engine` の結果には影響しない。
    #[test]
    fn run_child_dispatch() {
        run_child_if_requested();
    }

    // --- 冪等性: 複数回 install しても panic せず正常継続すること ---

    #[test]
    fn install_is_idempotent() {
        let (status, stdout_buf) = spawn_child("idempotent", None);
        assert!(
            status.success(),
            "idempotent install must exit successfully; status={status:?} stdout={stdout_buf}"
        );
        assert!(
            stdout_buf.contains("CHILD_IDEMPOTENT_OK"),
            "child must reach the success marker; stdout={stdout_buf}"
        );
    }

    // --- fail-fast の核心: メインスレッド panic がプロセスを終了させること ---

    #[test]
    fn main_thread_panic_aborts_process() {
        let (status, stdout_buf) = spawn_child("main_thread_panic", None);
        assert!(
            !status.success(),
            "child must not exit successfully; status={status:?} stdout={stdout_buf}"
        );
        #[cfg(unix)]
        assert_sigabrt(status, &stdout_buf);
    }

    // --- fail-fast の核心: ワーカースレッド panic も join を経ずプロセスを
    // 終了させること（既定の Rust 挙動〔当該スレッドのみ死ぬ〕からの逸脱を
    // 確認する ―― RECOVER-8 の「経路を問わず」の核心）。

    #[test]
    fn worker_thread_panic_aborts_process() {
        let (status, stdout_buf) = spawn_child("worker_thread_panic", None);
        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_WORKER_JOIN"),
            "fail-fast must abort before the worker's join() returns; stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child must not exit successfully; status={status:?} stdout={stdout_buf}"
        );
        #[cfg(unix)]
        assert_sigabrt(status, &stdout_buf);
    }

    // --- フックチェーンの核心: install() は直前のフックを必ず呼んでから
    // abort すること（TASK-97・RECOVER-6 の緊急応答退行防止。advisor 指摘対応
    // ―― SIGABRT のみの確認では「前フックを呼ばずに abort する」誤実装を
    // 検出できない）。

    #[test]
    fn install_calls_previous_hook_before_aborting() {
        let marker_path = std::env::temp_dir().join(format!(
            "engine-fail-fast-hook-chain-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_file(&marker_path);

        let (status, stdout_buf) = spawn_child(
            "hook_chain",
            Some((
                CHILD_MARKER_PATH_ENV,
                marker_path.to_str().expect("marker path is valid utf-8"),
            )),
        );

        assert!(
            !status.success(),
            "child must not exit successfully; status={status:?} stdout={stdout_buf}"
        );
        #[cfg(unix)]
        assert_sigabrt(status, &stdout_buf);

        let marker_contents = std::fs::read(&marker_path).unwrap_or_default();
        let _ = std::fs::remove_file(&marker_path);
        assert_eq!(
            marker_contents, b"PREV_HOOK_CALLED",
            "install() must invoke the previously-installed hook before aborting \
             (regression would silently drop TASK-97/RECOVER-6 emergency responses); \
             status={status:?} stdout={stdout_buf}"
        );
    }
}
