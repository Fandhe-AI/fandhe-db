//! 簡易クエリプロトコル（'Q'）本文の実行と応答整形を担う（TASK-73・WIRE-1）。
//!
//! 呼び出し文脈: `handshake::post_auth_loop` が UTF-8 検証済みの `'Q'` 本文を
//! 受け取った直後にここへ委譲する。責務境界は
//! (1) `engine::core::EngineCore::execute_sql_in_session` 呼び出し、
//! (2) 成功／失敗結果の wire メッセージへの整形（実バイト列生成は
//! [`crate::result_encoder`] に委譲）、(3) 接続単位 `SessionState` の
//! 受け渡しのみ。SQL の構文解釈・許可リスト判定・RLS 適用はすべて engine 側
//! （`engine::sql::allowlist::validate_sql`）に委ねる。
//!
//! **セミコロン区切りの複数文実行（WIRE-16・TASK-219・Issue #938）**: 1 つの
//! `'Q'` 本文にセミコロン区切りで複数の SQL 文が含まれる場合、
//! [`engine::sql::statement_splitter::split_statements`] へ分割を委譲する
//! （wire 層は SQL の字句知識を一切持たない）。各文は `RowDescription`／
//! `DataRow`*／`CommandComplete` を順に送出し、最後の文の応答にのみ
//! `ReadyForQuery` を付ける（[`run_statement`] の `finish` 引数）。途中の文が
//! エラーになった場合はそこで打ち切り、ErrorResponse＋`ReadyForQuery` を返して
//! 残りの文は実行しない。
//!
//! 実行方式は `engine::sql::statement_splitter::plan_multi_statement` が選ぶ
//! （Issue #1175）。書き込み系文（`INSERT`／`UPDATE`／`DELETE`／`TRUNCATE`。
//! `StatementEffect::Write`）が最後の 1 文だけ、または `BEGIN`／`COMMIT`／
//! `ROLLBACK` を含むメッセージ・トランザクション中のセッションは文ごとに順次実行し
//! （[`run_sequential_statements`]。従来どおり `check_write_placement` が書き込み位置を
//! 検査し、違反は `0A000`）、書き込みが最後以外にあるメッセージはメッセージ全体を
//! 1 つの暗黙トランザクションで原子的に実行する（[`run_implicit_transaction`]。
//! 途中でエラーになれば先行する書き込みも残らず、接続は `ReadyForQuery('I')` のまま）。
//! 詳細・既知の逸脱は `docs/design/wire-multi-statement.md` 参照。
//! 途中でエラーになった場合はセッション状態（`SET`／`CREATE FUNCTION` 等）も
//! メッセージ受信前の値へ巻き戻す（`SessionState` の `clone` を保持し、
//! 失敗時に復元する）。ただし順次実行でメッセージ内の明示トランザクションの境界
//! （`BEGIN`/`COMMIT`/`ROLLBACK`）を跨いだ場合、巻き戻し先はその境界の時点の
//! 状態へ更新する（SQL-31・TASK-221。`MessageSnapshot` 参照）。
//!
//! `COPY`（WIRE-17）は文単位で [`run_copy_statement`] が扱い、複数文メッセージの
//! 2 文目以降の `COPY` も CopyIn／CopyOut サブプロトコルへ入る（Issue #1175）。
//!
//! `INSERT` は wire 経由で受理する（TASK-82・SQL-10。`EngineCore::
//! execute_sql_in_session` が先頭トークンを見て `execute_insert_sql`（TASK-80）
//! へ委譲し `SqlOutcome::Insert` を返す。`crates/engine/src/core.rs` 参照）。
//! `INSERT`／`DELETE`／`UPDATE`／UPSERT に `RETURNING` 句を付けた場合は `SqlOutcome::
//! Returning` を返し、`respond_rows_with_tag` が `RowDescription`／`DataRow`*
//! に続けて `rows_affected`（`result.rows.len()` とは独立）由来の
//! `CommandComplete` タグを送出する（Issue #873・SQL-21）。
//! engine 側の `INSERT`（`sql::exec::execute_insert`）は行を常に
//! `Visibility::Private` で書き込む固定仕様であり、wire 認証経由の
//! `PolicyContext`（`auth::verify`）は `Public` ＋ 自テナントの `Private` を
//! 許可可視性とする（RLS-11・TASK-195。read-your-writes: 書いた本人が
//! 同一テナントの別セッションも含めて commit 済みの自分の行を読み戻せる）。
//! 他テナントの `Private` 行は引き続き不可視のまま
//! （`wire1_three_tenant_visibility_public_shared_own_private_visible` が
//! 回帰確認）。wire 経由で書いた `Private` 行は同一 wire セッションの
//! `SELECT` でも可視であり、書いた本人がその場で読み戻せる
//! （`wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant`
//! が契約を固定する。永続化自体は engine API 側の `Private` 可視
//! `PolicyContext` からも同じく確認できる）。

use std::io;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;
use engine::sql::SqlOutcome;

use crate::extended_query::LoopSignal;
use crate::result_encoder;
use crate::wire_stream::WireStream;

fn write_all<S: WireStream>(stream: &mut S, msg: &[u8]) -> io::Result<()> {
    stream.write_all(msg)
}

/// ErrorResponse を書いてから ReadyForQuery を書く（簡易クエリのエラーは接続を
/// 維持する。WIRE-8 の「切断」は拡張クエリプロトコル限定の既存契約であり、ここは
/// 変更しない）。`class` は `crate::handshake::write_error_response_io`（実体は
/// `crate::error_response::encode`）へそのまま渡り、severity・SQLSTATE の決定を
/// 横断写像へ一元化する（TASK-153・ERR-1・codex-review P1 指摘対応・PR #258）。
fn respond_error_and_ready<S: WireStream>(
    stream: &mut S,
    class: ErrorClass,
    message: &str,
    txn_status: engine::sql::transaction::TransactionStatus,
) -> io::Result<()> {
    crate::handshake::write_error_response_io(stream, class, message)?;
    crate::handshake::write_ready_for_query_io(stream, txn_status)
}

/// [`execute_and_respond`]・[`run_statement`] が使う「この応答の後に
/// `ReadyForQuery` を送出するか」の指定（WIRE-16）。複数文メッセージでは
/// 最後の文の応答にのみ [`Finish::ReadyForQuery`] を渡し、途中の文は
/// [`Finish::Continue`]（`ReadyForQuery` を送らず次の文へ進む）にする。
/// エラー応答（`respond_error_and_ready`）は `finish` に関係なく常に
/// `ReadyForQuery` を送る（途中エラーで打ち切るため、その時点で確定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    ReadyForQuery,
    Continue,
}

/// [`run_statement`] の結果。複数文オーケストレーション（[`execute_and_respond`]）
/// が「次の文へ進むか、打ち切ってセッション状態を巻き戻すか」を判定するために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementStatus {
    /// 応答（`RowDescription`/`DataRow`*/`CommandComplete`、`finish` に応じた
    /// `ReadyForQuery`）を書き終えた。
    Completed,
    /// ErrorResponse＋`ReadyForQuery` を書き終えた（`respond_error_and_ready`・
    /// エンコード失敗時のフォールバックのいずれか）。呼び出し元は以降の文を
    /// 実行せず、セッション状態を巻き戻す。
    Failed,
}

/// 簡易クエリ本文を実行し、成功／失敗いずれの場合も応答（`ReadyForQuery` 込み）を
/// 書き切る。呼び出し元は UTF-8 検証済みの `sql` のみを渡すこと（バイト列のまま
/// 渡さない。UTF-8 検証は `handshake::post_auth_loop` の責務）。
///
/// セミコロン区切りの複数文（WIRE-16・TASK-219）は
/// [`engine::sql::statement_splitter::split_statements`] へ分割を委譲し、
/// 単一文（[`engine::sql::statement_splitter::SplitOutcome::Single`]）は元の
/// SQL テキストを無加工のまま [`run_statement`] へ渡す ―― これにより単一文の
/// 既存挙動（応答バイト列・エラーコード・メッセージ）は構造的に不変のまま保たれる。
///
/// `COPY`（WIRE-17）は文単位で [`run_copy_statement`] へ振り分ける（Issue #1175）。
/// 複数文メッセージの 2 文目以降の `COPY` も、PostgreSQL と同じく CopyIn／CopyOut
/// サブプロトコルへ入り、続きの文を処理する。戻り値 [`LoopSignal::Closed`] は COPY 中に
/// Terminate を受信した場合で、呼び出し元（`handshake::post_auth_loop`）は接続ループを
/// 終了させる。
///
/// SQL 本文・テナント ID はログへ出さない（security.md P0）。
pub(crate) fn execute_and_respond<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> io::Result<LoopSignal> {
    // commit 成功から本関数が応答を書き終える（`ReadyForQuery` 送出含む）までの
    // 区間全体を覆う RAII ガード（RECOVER-5 (3)・codex-review P1・PR #246 指摘対応）。
    // 区間内で engine 側の書き込み系 commit が成功すると
    // `engine::recovery::commit_boundary` 内部のスレッドローカルフラグが立ち、
    // 本関数の全 return 経路（正常 return・panic 伝播いずれも）でこのガードが
    // drop される際に、フラグが立ったまま unwind 中であればプロセスを abort する
    // （wire-server は接続 1 本につきスレッド 1 つが直列にクエリを処理する
    // thread-per-connection モデルのため、スレッドローカルでの受け渡しが成立する。
    // `engine::recovery::commit_boundary` モジュールドキュメント参照）。
    // WIRE-16: 複数文メッセージでも本ガードは `'Q'` 本文 1 通全体を覆ったまま
    // 変更しない（1 メッセージにつき commit は高々 1 回――`Sequential` では書き込みを
    // 最後の 1 文に限る制約、暗黙トランザクションでは最後の文の成功後の
    // `commit_implicit` 1 回だけ〔モジュールドキュメント「原子性」節〕により保証される）。
    // 注意: `_response_boundary` を `let _ = ...`（無名束縛）に書き換えると即座に
    // drop され、この保護区間全体が無効化される（`ResponseBoundaryGuard` は
    // `#[must_use]`。関数末尾まで生存させるため必ずこの名前付き束縛のまま保つ）。
    let _response_boundary = engine::recovery::commit_boundary::ResponseBoundaryGuard::new();

    if sql.trim().is_empty() {
        write_all(stream, &result_encoder::encode_empty_query_response())?;
        crate::handshake::write_ready_for_query_io(stream, txn.status())?;
        return Ok(LoopSignal::Continue);
    }

    match engine::sql::statement_splitter::split_statements(sql) {
        Err(e) => {
            // 分割・位置検証のエラーも、明示トランザクション中なら `Failed` へ
            // 遷移させる（SQL-31・TASK-221。PR #1041 レビュー指摘: `Active` の
            // まま残すと後続の `COMMIT` が先行する書き込みを永続化してしまう）。
            respond_splitter_error(stream, txn, &e)?;
            Ok(LoopSignal::Continue)
        }
        Ok(engine::sql::statement_splitter::SplitOutcome::Single) => {
            if engine::sql::copy::is_copy_statement(sql) {
                return run_copy_statement(
                    stream,
                    engine,
                    ctx,
                    session,
                    txn,
                    sql,
                    Finish::ReadyForQuery,
                )
                .map(copy_step_to_signal);
            }
            run_statement(
                stream,
                engine,
                ctx,
                session,
                txn,
                sql,
                Finish::ReadyForQuery,
            )
            .map(|_| LoopSignal::Continue)
        }
        Ok(engine::sql::statement_splitter::SplitOutcome::Empty) => {
            write_all(stream, &result_encoder::encode_empty_query_response())?;
            crate::handshake::write_ready_for_query_io(stream, txn.status())?;
            Ok(LoopSignal::Continue)
        }
        Ok(engine::sql::statement_splitter::SplitOutcome::Statements(stmts)) => {
            // セッションが `Idle` でない（明示トランザクション中・`Failed`）場合は
            // 暗黙トランザクションにせず既存の逐次実行へ倒す（`Failed` では個々の文が
            // `25P02` を返す）。
            let session_in_txn = txn.status() != engine::sql::transaction::TransactionStatus::Idle;
            match engine::sql::statement_splitter::plan_multi_statement(&stmts, session_in_txn) {
                Err(e) => {
                    respond_splitter_error(stream, txn, &e)?;
                    Ok(LoopSignal::Continue)
                }
                Ok(engine::sql::statement_splitter::MultiStatementPlan::Sequential) => {
                    run_sequential_statements(stream, engine, ctx, session, txn, &stmts)
                }
                Ok(engine::sql::statement_splitter::MultiStatementPlan::ImplicitTransaction) => {
                    run_implicit_transaction(stream, engine, ctx, session, txn, &stmts)
                }
            }
        }
    }
}

/// [`crate::copy::CopyStep`] を接続ループへの合図へ写像する（`Closed` のみ接続終了）。
fn copy_step_to_signal(step: crate::copy::CopyStep) -> LoopSignal {
    match step {
        crate::copy::CopyStep::Closed => LoopSignal::Closed,
        crate::copy::CopyStep::Completed | crate::copy::CopyStep::Failed => LoopSignal::Continue,
    }
}

/// 複数文メッセージを 1 文ずつ順次実行する（[`engine::sql::statement_splitter::
/// MultiStatementPlan::Sequential`]。文ごとの autocommit、または `BEGIN` を含む
/// メッセージ・トランザクション中のセッション）。途中の文がエラーになったらそこで
/// 打ち切り、セッション状態を直近のトランザクション境界へ巻き戻す
/// （[`MessageSnapshot`]）。
fn run_sequential_statements<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmts: &[&str],
) -> io::Result<LoopSignal> {
    // 途中の文がエラーになった場合に巻き戻すためのスナップショット
    // （`SET`／`CREATE FUNCTION` の暗黙ロールバック）。単一文経路
    // （`Single`）はこの clone を行わないため、既存の単一文レイテンシ・
    // コストは不変。トランザクション状態自体（`txn`）は巻き戻さない ――
    // `BEGIN` 済みのトランザクションは、途中の文がエラーになれば `Failed`
    // へ遷移したまま残り、次の `ROLLBACK` で閉じる契約
    // （`sql::transaction` モジュールドキュメント参照）。巻き戻し先は
    // メッセージ内の直近のトランザクション境界へ更新する
    // （[`MessageSnapshot`] 参照。PR #1041 レビュー指摘）。
    let mut snapshot = MessageSnapshot::new(session);
    let last_index = stmts.len().saturating_sub(1);
    for (i, stmt) in stmts.iter().enumerate() {
        let finish = if i == last_index {
            Finish::ReadyForQuery
        } else {
            Finish::Continue
        };
        let before = txn.status();
        let status = if engine::sql::copy::is_copy_statement(stmt) {
            match run_copy_statement(stream, engine, ctx, session, txn, stmt, finish)? {
                crate::copy::CopyStep::Completed => StatementStatus::Completed,
                crate::copy::CopyStep::Failed => StatementStatus::Failed,
                crate::copy::CopyStep::Closed => return Ok(LoopSignal::Closed),
            }
        } else {
            run_statement(stream, engine, ctx, session, txn, stmt, finish)?
        };
        match status {
            StatementStatus::Completed => {
                snapshot.after_completed(before, txn.status(), session);
            }
            StatementStatus::Failed => {
                // ErrorResponse＋ReadyForQuery は run_statement 内で
                // 送出済み。残りの文は実行せず、セッション状態を復元する。
                snapshot.restore_after_failure(before, txn.status(), session);
                return Ok(LoopSignal::Continue);
            }
        }
    }
    Ok(LoopSignal::Continue)
}

/// `BEGIN` を含まず、トランザクション外の書き込みが最後以外にある複数文メッセージを、
/// 1 つの暗黙トランザクションで原子的に実行する（Issue #1175・WIRE-16・SQL-31・
/// RECOVER-12。[`engine::sql::statement_splitter::MultiStatementPlan::
/// ImplicitTransaction`]）。
///
/// 開始点はメッセージの先頭（最初の書き込みの直前ではない）。メッセージ全体が 1 つの
/// トランザクションになり、`SET`・カーソル・読み取りの意味が一様になる代わりに、
/// 先頭の読み取りの間も単一ライタのゲートを保持する（保持時間は `max_duration` と
/// 文数上限 16 で有界）。commit は最後の文の応答を書く**前**に 1 回だけ行う
/// （[`run_statement_as`] の [`StatementRole::ImplicitFinal`]）。
///
/// 失敗経路はすべて `Idle`（write txn を abort し、ライタを解放）に収束する:
/// 途中エラーは `SessionTransaction::fail` が暗黙トランザクションを `Idle` へ戻し、
/// エラー応答は `ReadyForQuery('I')` と一緒に送られる。セッション状態
/// （`SET`／`CREATE FUNCTION`）はメッセージ受信前のスナップショットへ**無条件に**
/// 復元する（[`MessageSnapshot`] は `Active` → `Idle` の遷移で復元を省くため使わない）。
/// どの return 経路でも、戻る直前に暗黙トランザクションが残っていれば abort する。
fn run_implicit_transaction<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmts: &[&str],
) -> io::Result<LoopSignal> {
    let snapshot = session.clone();
    if let Err(e) = engine.begin_implicit_transaction(txn, session) {
        // ライタゲートの待機上限超過（`55P03`）等。1 文も実行しない。
        respond_error_and_ready(stream, e.error_class(), &e.client_message(), txn.status())?;
        return Ok(LoopSignal::Continue);
    }
    let result = run_implicit_statements(stream, engine, ctx, session, txn, stmts, &snapshot);
    // 後処理の一元化: 暗黙トランザクションを残したまま次のメッセージへ進まない
    // （成功時は `commit_implicit` が、失敗時は `fail` が既に `Idle` へ戻している。
    // `io::Error` による中断や想定外の経路の最後の砦）。
    txn.abort_implicit();
    result
}

/// [`run_implicit_transaction`] の本体（文の逐次実行と失敗時のセッション復元）。
fn run_implicit_statements<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmts: &[&str],
    snapshot: &SessionState,
) -> io::Result<LoopSignal> {
    let last_index = stmts.len().saturating_sub(1);
    for (i, stmt) in stmts.iter().enumerate() {
        let is_last = i == last_index;
        if engine::sql::copy::is_copy_statement(stmt) {
            // トランザクション内の COPY は未対応（`0A000`）。`run_copy_statement` が
            // 暗黙トランザクションを `Idle` へ戻し、`ReadyForQuery('I')` を送る。
            // 原子性を優先し、CopyIn へは入らずメッセージ全体をロールバックする。
            let step = run_copy_statement(
                stream,
                engine,
                ctx,
                session,
                txn,
                stmt,
                Finish::ReadyForQuery,
            )?;
            *session = snapshot.clone();
            return Ok(copy_step_to_signal(step));
        }
        let (finish, role) = if is_last {
            (Finish::ReadyForQuery, StatementRole::ImplicitFinal)
        } else {
            (Finish::Continue, StatementRole::ImplicitIntermediate)
        };
        match run_statement_as(stream, engine, ctx, session, txn, stmt, finish, role)? {
            StatementStatus::Completed => {
                if !is_last && !txn.is_implicit_active() {
                    // 想定外: 途中の文の後に暗黙トランザクションが失われている。
                    // 後続の書き込みが autocommit で実行されないよう、ここで打ち切る
                    // （fail-closed）。
                    respond_error_and_ready(
                        stream,
                        ErrorClass::InternalError,
                        "internal error",
                        txn.status(),
                    )?;
                    *session = snapshot.clone();
                    return Ok(LoopSignal::Continue);
                }
            }
            StatementStatus::Failed => {
                // ErrorResponse＋ReadyForQuery('I') は送出済み。先行する書き込みは
                // `fail` が破棄している。セッション状態をメッセージ受信前へ戻す。
                *session = snapshot.clone();
                return Ok(LoopSignal::Continue);
            }
        }
    }
    Ok(LoopSignal::Continue)
}

/// 分割済みの `COPY` 1 文を実行する（Issue #1175。従来は `handshake::post_auth_loop` が
/// メッセージ全文を覗いて `crate::copy::run` へ分岐していたが、複数文メッセージの
/// 2 文目以降の `COPY` を扱うため文単位へ移した）。
///
/// セッションの状態に応じて分岐する（SQL-31・TASK-221。PR #1041 レビュー指摘:
/// `Failed` 中の COPY も autocommit として実行させない）:
/// - `Idle`・明示トランザクション中の `InTransaction`（Issue #1179）:
///   [`crate::copy::run`] へ委譲する。
/// - 暗黙トランザクション中の `InTransaction`: 未対応。`fail()` の後に `0A000`。
///   暗黙トランザクションでは `fail()` が `Idle` へ戻すので `ReadyForQuery('I')`。
/// - `Failed`: 他の文と同じく `25P02`（期限切れ未報告なら `54000`）で拒否する。
fn run_copy_statement<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmt: &str,
    finish: Finish,
) -> io::Result<crate::copy::CopyStep> {
    use engine::sql::transaction::TransactionStatus;
    match txn.status() {
        TransactionStatus::Idle => {
            crate::copy::run(stream, engine, ctx, session, txn, stmt, finish)
        }
        // 明示トランザクション中の COPY は共有書き込みトランザクションへ委譲する
        // （Issue #1179。`COPY FROM STDIN` は複数行 INSERT、`COPY ... TO STDOUT` は
        // 読み取り。失敗時は `copy::run` がトランザクションを `Failed` へ遷移させる）。
        TransactionStatus::InTransaction if !txn.is_implicit_active() => {
            crate::copy::run(stream, engine, ctx, session, txn, stmt, finish)
        }
        // 暗黙トランザクション（Issue #1175）中の COPY は未対応（原子性を優先し、
        // CopyIn へは入らずメッセージ全体をロールバックする）。
        TransactionStatus::InTransaction => {
            txn.fail();
            respond_error_and_ready(
                stream,
                ErrorClass::FeatureNotSupported,
                "COPY is not supported inside a transaction block",
                txn.status(),
            )?;
            Ok(crate::copy::CopyStep::Failed)
        }
        TransactionStatus::Failed => {
            let err = txn.take_failed_error();
            respond_error_and_ready(
                stream,
                err.error_class(),
                &err.client_message(),
                txn.status(),
            )?;
            Ok(crate::copy::CopyStep::Failed)
        }
    }
}
/// 複数文メッセージ（WIRE-16）の途中エラー時に巻き戻すセッション状態の
/// スナップショット（SQL-31・TASK-221。PR #1041 レビュー指摘）。
///
/// メッセージ受信前の状態を無条件に復元すると、同じメッセージ内の `ROLLBACK`
/// （または失敗した `COMMIT`）が `BEGIN` 時点の状態へ戻した後に後続文が失敗した
/// 場合、トランザクション中の `SET`／`CREATE FUNCTION` が `Idle` のセッションに
/// 復活してしまう。PostgreSQL の暗黙トランザクションと同じく、巻き戻しの範囲は
/// 「メッセージ内の直近のトランザクション境界（`BEGIN`/`COMMIT`/`ROLLBACK`）以降」
/// に限る。
struct MessageSnapshot {
    session: SessionState,
}

impl MessageSnapshot {
    fn new(session: &SessionState) -> Self {
        Self {
            session: session.clone(),
        }
    }

    /// 文が成功した直後に呼ぶ。トランザクション状態が変わった（＝文がトランザ
    /// クション境界を跨いだ）場合に限り、巻き戻し先をその時点の状態へ更新する
    /// （状態が変わらない文では clone しない）。
    fn after_completed(
        &mut self,
        before: engine::sql::transaction::TransactionStatus,
        after: engine::sql::transaction::TransactionStatus,
        session: &SessionState,
    ) {
        if before != after {
            self.session = session.clone();
        }
    }

    /// 文が失敗した直後に呼ぶ。失敗した文自身がトランザクションを閉じた場合
    /// （`Active`/`Failed` → `Idle`。失敗した `COMMIT` は `BEGIN` 時点の状態を
    /// 既に復元している）は、その復元結果を上書きしない。それ以外は直近の境界の
    /// 状態へ巻き戻す。
    fn restore_after_failure(
        self,
        before: engine::sql::transaction::TransactionStatus,
        after: engine::sql::transaction::TransactionStatus,
        session: &mut SessionState,
    ) {
        use engine::sql::transaction::TransactionStatus;
        if before != TransactionStatus::Idle && after == TransactionStatus::Idle {
            return;
        }
        *session = self.session;
    }
}

/// 複数文メッセージの分割・位置検証エラーの応答（SQL-31・TASK-221）。
/// 明示トランザクションが `Failed` なら、個々の文と同じく `25P02`（期限切れの
/// 未報告分があれば `54000`）を返す。`Active` なら `Failed` へ遷移させてから
/// 元のエラーを返す（`Active` のまま残すと後続の `COMMIT` が先行する書き込みを
/// 永続化してしまう。PR #1041 レビュー指摘）。
fn respond_splitter_error<S: WireStream>(
    stream: &mut S,
    txn: &mut SessionTransaction<'_>,
    e: &engine::sql::statement_splitter::MultiStatementError,
) -> io::Result<()> {
    if txn.status() == engine::sql::transaction::TransactionStatus::Failed {
        let failed = txn.take_failed_error();
        return respond_error_and_ready(
            stream,
            failed.error_class(),
            &failed.client_message(),
            txn.status(),
        );
    }
    txn.fail();
    respond_error_and_ready(stream, e.error_class(), &e.client_message(), txn.status())
}

/// 複数文実行の 1 文を実行し、応答（`finish` に応じた `ReadyForQuery` の有無）を
/// 書き切る。[`execute_and_respond`] の「outcome を決定する区間」（TASK-97・
/// RECOVER-6・ERR-1）をここへ抽出したもの ―― 単一文経路（`SplitOutcome::Single`）
/// でも複数文経路でも 1 文につき 1 回呼ばれ、緊急応答の登録・panic 注入点の
/// 位置関係は移設前と同一のまま保たれる。
///
/// TASK-97（対象ビヘイビア: RECOVER-6・ERR-1、codex-review Medium 指摘対応・
/// PR #90）: 登録はブロックスコープで「outcome を決定する区間」だけを覆う
/// ―― ブロック終端（`engine.execute_sql_in_session` の呼び出し直後）で
/// レキシカルに drop され、以降の応答書き込み（`match outcome { .. }` 側）
/// には一切及ばない。これは構造的な安全境界であり、外してはならない ――
/// 将来 commit を伴う書き込み経路が接続された場合、応答書き込みの途中
/// （例: `respond_query_result` が行を書き出している最中）で panic すると、
/// その時点で commit は既に pending 済みのため、もし登録がまだ有効なら
/// 緊急応答バイト列が「書きかけの通常応答フレームの上に」追記されてしまう
/// （[`EmergencyResponseRegistration`] のドキュメントが警告する
/// 「フレーム途中への緊急応答混入・二重応答」そのもの）。`must_use` の
/// 束縛忘れ検出を利用し、`let _ =` に書き換えて即座に drop してしまう事故を
/// 防ぐため、束縛名を `_emergency_registration` とし、`drop()` の明示呼び出し
/// には頼らずブロックの終わりに任せる（呼び出し忘れの手動 `drop` はその後に
/// コードが追加されると孤立しうるが、ブロックスコープはコードの追加位置に
/// 関わらず構造的に保たれる）。
///
/// 登録（eager）と送出（emergency_send_decision による commit 成功フラグの
/// 世代一致判定）は別軸である ―― ここでの登録はブロック内で panic が
/// 起きたら常に送られることを意味しない。詳細は
/// [`build_emergency_response_bytes`] のドキュメント参照。
///
/// `INSERT` 等の分岐先決定は engine 側（`EngineCore::execute_sql_in_session`。
/// TASK-82・SQL-10）に一元化する。wire 層はここで構文種別ごとに分岐しない
/// （モジュール冒頭コメント参照）。
fn run_statement<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmt_sql: &str,
    finish: Finish,
) -> io::Result<StatementStatus> {
    run_statement_as(
        stream,
        engine,
        ctx,
        session,
        txn,
        stmt_sql,
        finish,
        StatementRole::Standalone,
    )
}

/// 文が暗黙トランザクション（Issue #1175）の中でどの位置にあるか。commit を行う
/// 区間と緊急応答の登録区間を一致させるために [`run_statement_as`] が使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementRole {
    /// 暗黙トランザクション外の文（単一文・逐次実行の各文）。
    Standalone,
    /// 暗黙トランザクションの最後以外の文。commit は起きないので、緊急応答の登録・
    /// 障害注入点は不要（commit 前の panic では緊急応答は送られない）。
    ImplicitIntermediate,
    /// 暗黙トランザクションの最後の文。文の実行に続けて `commit_implicit` を
    /// **緊急応答の登録区間の内側**で行う。これにより RECOVER-6（commit 後の panic
    /// への緊急応答）が commit 点を覆い、「1 メッセージにつき commit は高々 1 回」も
    /// 保たれる（`_response_boundary` は変えない）。commit に失敗した場合は
    /// `CommandComplete` を送らず ErrorResponse＋`ReadyForQuery('I')` を返す
    /// （commit を最後の応答より前に行う PostgreSQL と同じ順序）。
    ImplicitFinal,
}

/// [`run_statement`] の本体（[`StatementRole`] で暗黙トランザクション内の位置を指定する）。
#[allow(clippy::too_many_arguments)]
fn run_statement_as<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    stmt_sql: &str,
    finish: Finish,
    role: StatementRole,
) -> io::Result<StatementStatus> {
    let outcome = match role {
        StatementRole::Standalone => execute_with_emergency_registration(stream, || {
            engine.execute_sql_in_txn(ctx, session, txn, stmt_sql)
        }),
        StatementRole::ImplicitIntermediate => {
            engine.execute_sql_in_txn(ctx, session, txn, stmt_sql)
        }
        StatementRole::ImplicitFinal => execute_with_emergency_registration(stream, || {
            let outcome = engine.execute_sql_in_txn(ctx, session, txn, stmt_sql)?;
            txn.commit_implicit(session)?;
            Ok(outcome)
        }),
    };

    match outcome {
        Ok(outcome) => match map_outcome(outcome) {
            OutcomeResponse::Rows { result, shape } => {
                let sent = result.rows.len();
                let tag = shape.render(sent);
                respond_rows_with_tag(stream, &result, &tag, finish, txn)
            }
            OutcomeResponse::Command { tag } => respond_command_complete(stream, &tag, finish, txn),
        },
        Err(e) => {
            // `engine::core::EngineCore::execute_sql_in_txn`／
            // `execute_parsed_in_txn` はエラー時に必ず `txn` の状態を確定させて
            // から返す（`Active` 中は `fail()` 済み・`Failed` 中の再エラーは
            // 据え置き）ため、ここで読む `txn.status()` は直前のエラーを反映
            // 済み（WIRE-19・SQL-31・TASK-221・PR #1041 レビュー指摘 P1）。
            respond_error_and_ready(stream, e.error_class(), &e.client_message(), txn.status())?;
            Ok(StatementStatus::Failed)
        }
    }
}

/// 緊急応答（RECOVER-6）の登録本体。平文・TLS 双方の接続で共有する
/// （[`execute_with_emergency_registration`]・`crate::copy::finish_copy_from`
/// の双方から呼ぶ。Issue #1080 で TLS 接続向けに一本化した）。
///
/// `WireStream::emergency_response_channel` が返す組（送出形式済み
/// バイト列＋書き込み先の生ソケット複製）をそのまま
/// `engine::recovery::panic_hook::EmergencyResponseRegistration::register`
/// へ渡す。TLS 接続では `emergency_response_channel`（[`crate::tls::stream::
/// TlsStream`] の上書き実装）が緊急応答を TLS レコードとして暗号化済みの
/// バイト列を返すため、この関数自身は送出形式を意識しない。
pub(crate) fn register_emergency_response<S: WireStream + ?Sized>(
    stream: &S,
) -> Option<engine::recovery::panic_hook::EmergencyResponseRegistration> {
    let response_bytes = cached_emergency_response_bytes()?;
    let (bytes, clone) = stream.emergency_response_channel(response_bytes)?;
    Some(
        engine::recovery::panic_hook::EmergencyResponseRegistration::register(
            bytes,
            clone,
            crate::limits::EMERGENCY_RESPONSE_WRITE_TIMEOUT,
        ),
    )
}

/// TASK-97（対象ビヘイビア: RECOVER-6・ERR-1、codex-review Medium 指摘対応・
/// PR #90）の「登録ブロック」を関数として切り出したもの（Issue #934・#933 の
/// Execute（拡張クエリプロトコル）が [`engine::core::EngineCore::
/// execute_parsed_in_session`] 呼び出しでも同じ緊急応答登録・panic 注入点の
/// 位置関係を再利用するための共有本体。簡易クエリ（[`run_statement`]）・
/// 拡張クエリの Execute（`crate::extended_query::handle_execute`）はいずれも
/// 「outcome を決定する区間」をこの関数の `f` 引数へ委譲することで、登録
/// （eager）と panic 注入位置の契約を 1 箇所に保つ——`f` の呼び出し完了直後
/// （ブロック終端）で `_emergency_registration` がレキシカルに drop されるため、
/// 呼び出し元がこの関数から返った outcome を使って応答を組み立てている間は
/// 緊急応答チャネルへの登録が既に外れている（[`run_statement`] 旧実装の
/// コメントが警告していた「書きかけの通常応答フレームへの緊急応答混入」を
/// 防ぐ構造は不変のまま）。
pub(crate) fn execute_with_emergency_registration<S: WireStream>(
    stream: &mut S,
    f: impl FnOnce() -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError>,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let _emergency_registration = register_emergency_response(stream);
    let outcome = f();
    // Issue #705（テスト専用・feature `fault-injection` 限定）: 直前行の
    // `outcome` を「登録ブロック」の終端（`_emergency_registration` が
    // drop される直前）でだけ検査し、commit 後 panic を注入できる唯一の
    // 位置に置く。この関数を抜けて呼び出し元が outcome を使い始めると
    // `_emergency_registration` は既に drop 済みで緊急応答は送られなく
    // なる（上記ドキュメント「outcome を決定する区間」参照）ため、注入点を
    // ここより後ろへ移動してはならない。feature 無効時はこの呼び出し
    // ごとコンパイルされず、既定ビルドの挙動・コード生成は完全に不変。
    #[cfg(feature = "fault-injection")]
    crate::fault_injection::maybe_panic_after_commit(&outcome);
    outcome
}

/// [`SqlOutcome`] の行を返す応答が確定する際、`CommandComplete` タグの数値部分を
/// どう決めるかの区別（Issue #934。拡張クエリプロトコルの Execute が `max_rows`
/// で行を分割送出できるようになったことで、簡易クエリ〔常に全行を 1 回で送る〕
/// とは「タグの count をどの時点の行数から取るか」が食い違いうるため、`map_outcome`
/// の戻り値へ埋め込んで両呼び出し元（[`run_statement`]・`crate::extended_query::
/// handle_execute`）が共有する）。
pub(crate) enum TagShape {
    /// `prefix` と実際に送出した行数（`render` の `sent` 引数）からタグを組み立てる
    /// （`SELECT`。拡張クエリの分割送出では PostgreSQL の `PortalRun` と同じく
    /// 「その回の Execute で実際に送った行数」を使う）。
    Dynamic(&'static str),
    /// 常に固定文字列（`EXPLAIN`。行を返すが件数を持たないタグ）。
    Fixed(String),
    /// タグの数値部分も固定（`INSERT 0 <n>`・`RETURNING` の `rows_affected`）。
    /// `result.rows.len()` とは独立の値であり、分割送出の影響を受けない
    /// （`ReturningOutcome` のドキュメント参照）。
    FixedTag(String),
}

impl TagShape {
    /// この応答が確定する（`CommandComplete` を送る）時点でタグ文字列を組み立てる。
    /// `sent` は [`TagShape::Dynamic`] にのみ効き、それ以外は無視される。
    pub(crate) fn render(&self, sent: usize) -> String {
        match self {
            TagShape::Dynamic(prefix) => format!("{prefix} {sent}"),
            TagShape::Fixed(s) | TagShape::FixedTag(s) => s.clone(),
        }
    }
}

/// [`SqlOutcome`] を「行を返す応答」か「`CommandComplete` 単独応答」かへ写像する
/// （Issue #934。旧 [`run_statement`] の `match outcome { .. }` 本体をここへ抽出し、
/// 拡張クエリプロトコルの Execute（`crate::extended_query::handle_execute`）とも
/// 共有する。SQL 種別ごとの `CommandComplete` タグ文言・分岐は本関数が唯一の
/// 情報源であり、`run_statement`・`handle_execute` の双方がこれ以外の場所で
/// タグを組み立てない）。
pub(crate) enum OutcomeResponse {
    Rows {
        result: engine::sql::exec::QueryResult,
        shape: TagShape,
    },
    Command {
        tag: String,
    },
}

pub(crate) fn map_outcome(outcome: SqlOutcome) -> OutcomeResponse {
    match outcome {
        SqlOutcome::Query(result) => OutcomeResponse::Rows {
            result,
            shape: TagShape::Dynamic("SELECT"),
        },
        // TASK-78（SQL-6）: `EXPLAIN` は検索本体を実行しない別応答だが、行の
        // 形（`QUERY PLAN` 単一列・複数 `Cell::Text` 行）は通常の検索 SELECT と
        // 同じ `RowDescription`/`DataRow` エンコードを再利用できる（`ColumnMeta`/
        // `ResultRow` の汎用性による）。CommandComplete タグのみ pg 互換の
        // `EXPLAIN` に差し替える（件数を持たない固定タグ）。
        SqlOutcome::Explain(result) => OutcomeResponse::Rows {
            result,
            shape: TagShape::Fixed("EXPLAIN".to_string()),
        },
        SqlOutcome::SetSearchMode(_) => OutcomeResponse::Command {
            tag: "SET".to_string(),
        },
        SqlOutcome::CreateFunction { .. } => OutcomeResponse::Command {
            tag: "CREATE FUNCTION".to_string(),
        },
        // TASK-82（SQL-10）: `INSERT`（行形・ファイル形いずれも
        // `exec::InsertOutcome::rows_affected` に書き込み件数を保持する。
        // `sql/exec.rs` ドキュメント参照）の応答を pg 互換の `CommandComplete`
        // タグ `INSERT <oid> <rows>` へ整形する。OID 機構は本実装に無いため
        // 固定で `0` を使う（pg プロトコルの規範）。
        SqlOutcome::Insert(outcome) => OutcomeResponse::Command {
            tag: format!("INSERT 0 {}", outcome.rows_affected),
        },
        // TASK-193（SQL-22）: `TRUNCATE TABLE`（`exec::TruncateOutcome`。削除件数を
        // 一切返さない契約）の応答を pg 互換の `CommandComplete` タグ
        // `TRUNCATE TABLE`（件数を持たない固定タグ）へ整形する。
        SqlOutcome::Truncate(_) => OutcomeResponse::Command {
            tag: "TRUNCATE TABLE".to_string(),
        },
        // SQL-18（TASK-191・#867）: `DELETE`（単一行・`id` 等価指定形。
        // `exec::DeleteOutcome::rows_affected` は自テナント削除件数
        // `0`／`1` のみを保持する）の応答を pg 互換の `CommandComplete` タグ
        // `DELETE <rows>` へ整形する。
        SqlOutcome::Delete(outcome) => OutcomeResponse::Command {
            tag: format!("DELETE {}", outcome.rows_affected),
        },
        // Issue #873（SQL-21）: `RETURNING` 句付き `INSERT`／`DELETE` の応答。
        // `RowDescription`／`DataRow`* は通常の検索 SELECT・`EXPLAIN` と同じ
        // 経路で組み立てるが、`CommandComplete` タグの件数は
        // `result.rows.len()`（RLS 再判定後に絞られた投影行数）ではなく
        // `outcome.rows_affected`（実際に変更した行数）を使う——両者は書き込み
        // 本人にも不可視な行がある場合に一致しないことが契約上ありうるため
        // `TagShape::FixedTag` で固定する（分割送出の影響を受けない）。
        SqlOutcome::Returning(outcome) => {
            let tag = match outcome.command {
                engine::sql::returning::DmlCommand::Insert => {
                    format!("INSERT 0 {}", outcome.rows_affected)
                }
                engine::sql::returning::DmlCommand::Update => {
                    format!("UPDATE {}", outcome.rows_affected)
                }
                engine::sql::returning::DmlCommand::Delete => {
                    format!("DELETE {}", outcome.rows_affected)
                }
            };
            OutcomeResponse::Rows {
                result: outcome.result,
                shape: TagShape::FixedTag(tag),
            }
        }
        // `UPDATE`（単一行・id 指定形。SQL-17・TASK-191、Issue #865。述語形。
        // SQL-19・TASK-192、Issue #871）の応答を pg 互換の `CommandComplete`
        // タグ `UPDATE <rows>` へ整形する。
        SqlOutcome::Update(outcome) => OutcomeResponse::Command {
            tag: format!("UPDATE {}", outcome.rows_affected),
        },
        // SQL-31（TASK-221）: 明示トランザクション制御文の応答を pg 互換の
        // `CommandComplete` タグへ整形する（いずれも件数を持たない固定タグ）。
        SqlOutcome::Begin => OutcomeResponse::Command {
            tag: "BEGIN".to_string(),
        },
        SqlOutcome::Commit => OutcomeResponse::Command {
            tag: "COMMIT".to_string(),
        },
        SqlOutcome::Rollback => OutcomeResponse::Command {
            tag: "ROLLBACK".to_string(),
        },
        // SQL-23・TASK-85・TASK-202（Issue #899）: `CREATE TABLE`
        // （`exec::ddl::CreateTableOutcome`。行数・件数を一切持たない契約）の
        // 応答を pg 互換の `CommandComplete` タグ `CREATE TABLE`（件数を持たない
        // 固定タグ。`TRUNCATE TABLE` と同じ設計）へ整形する。
        SqlOutcome::CreateTable(_) => OutcomeResponse::Command {
            tag: "CREATE TABLE".to_string(),
        },
        // SQL-23・TASK-203（Issue #902）: `DROP TABLE`（`sql::ddl::
        // DropTableOutcome` は削除件数を一切持たない）の応答を pg 互換の
        // `CommandComplete` タグ `DROP TABLE`（件数を持たない固定タグ。
        // `TRUNCATE TABLE` と同じ設計）へ整形する。
        SqlOutcome::DropTable(_) => OutcomeResponse::Command {
            tag: "DROP TABLE".to_string(),
        },
        // WIRE-15・TASK-218: `DECLARE`／`CLOSE` は件数を持たない固定タグの
        // `CommandComplete`。`FETCH` は検索 SELECT と同じ `RowDescription`／
        // `DataRow`* エンコードを再利用し、タグの数値部分は実際に送出した
        // 行数（分割送出時は 1 回の Execute で送った行数）にする。
        SqlOutcome::DeclareCursor => OutcomeResponse::Command {
            tag: "DECLARE CURSOR".to_string(),
        },
        SqlOutcome::Fetch(result) => OutcomeResponse::Rows {
            result,
            shape: TagShape::Dynamic("FETCH"),
        },
        SqlOutcome::CloseCursor => OutcomeResponse::Command {
            tag: "CLOSE CURSOR".to_string(),
        },
        // TASK-202（SQL-23。Issue #900）: `ALTER TABLE ADD COLUMN` の応答を
        // pg 互換の `CommandComplete` タグ `ALTER TABLE`（件数を持たない固定
        // タグ）へ整形する（`Truncate` と同じ設計）。
        SqlOutcome::AlterTable(_) => OutcomeResponse::Command {
            tag: "ALTER TABLE".to_string(),
        },
        // TABLE-18・SQL-23・TASK-205（Issue #909）: `CREATE VIEW`／`DROP VIEW`
        // （`sql::ddl::CreateViewOutcome`／`DropViewOutcome` はいずれも件数を
        // 持たない）の応答を pg 互換の `CommandComplete` タグ（件数を持たない
        // 固定タグ。`DROP TABLE`・`TRUNCATE TABLE` と同じ設計）へ整形する。
        SqlOutcome::CreateView(_) => OutcomeResponse::Command {
            tag: "CREATE VIEW".to_string(),
        },
        SqlOutcome::DropView(_) => OutcomeResponse::Command {
            tag: "DROP VIEW".to_string(),
        },
        // TASK-206・INDEX-7・SQL-23（Issue #908）: 索引宣言 DDL も件数を持たない
        // pg 互換の固定タグへ整形する（`CREATE VIEW`／`DROP VIEW` と同じ設計）。
        SqlOutcome::CreateIndex(_) => OutcomeResponse::Command {
            tag: "CREATE INDEX".to_string(),
        },
        SqlOutcome::DropIndex(_) => OutcomeResponse::Command {
            tag: "DROP INDEX".to_string(),
        },
    }
}

/// 行を返さない `CommandComplete` 単独応答（`SET`／`CREATE FUNCTION`／
/// `INSERT`／`TRUNCATE TABLE`／`DELETE`／`UPDATE`）の共通本体。`finish` が
/// [`Finish::Continue`] のときは `ReadyForQuery` を送らず、複数文の次の文へ
/// 制御を返す。
fn respond_command_complete<S: WireStream>(
    stream: &mut S,
    tag: &str,
    finish: Finish,
    txn: &mut SessionTransaction<'_>,
) -> io::Result<StatementStatus> {
    respond_command_complete_with(
        stream,
        tag,
        finish,
        txn,
        result_encoder::encode_command_complete,
    )
}

/// [`respond_command_complete`] の本体。`encode` はエンコード失敗経路を単体
/// テストから注入するための差し替え点で、production は常に
/// [`result_encoder::encode_command_complete`] を渡す。
///
/// エンコード失敗は、文自体は実行済み（`INSERT`／`TRUNCATE` 等の書き込みが共有
/// 書き込みトランザクションに積まれている）のにクライアントへはエラーを返す
/// 経路であるため、`respond_rows_with_tag` と同じく明示トランザクション中なら
/// `Failed` へ遷移させる（SQL-31・TASK-221。PR #1041 レビュー指摘: `Active` の
/// まま残すと後続の `COMMIT` が書き込みを永続化してしまう）。
fn respond_command_complete_with<S: WireStream>(
    stream: &mut S,
    tag: &str,
    finish: Finish,
    txn: &mut SessionTransaction<'_>,
    encode: impl FnOnce(&str) -> Result<Vec<u8>, result_encoder::EncodeError>,
) -> io::Result<StatementStatus> {
    match encode(tag) {
        Ok(msg) => {
            write_all(stream, &msg)?;
            if finish == Finish::ReadyForQuery {
                crate::handshake::write_ready_for_query_io(stream, txn.status())?;
            }
            Ok(StatementStatus::Completed)
        }
        Err(_) => {
            txn.fail();
            respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode command complete response",
                txn.status(),
            )?;
            Ok(StatementStatus::Failed)
        }
    }
}

/// 緊急応答（TASK-97・RECOVER-6、対象ビヘイビア ERR-1）の事前エンコード済み
/// バイト列を組み立てる。
///
/// `crate::error_response::encode_with_detail`（TASK-153・ERR-1・ERR-5）で
/// 通常応答と同じ `S`/`C`/`M` に加え `D`（detail）＝
/// `crate::error_response::MAY_BE_COMMITTED_DETAIL` を組み立てる。クライアントは
/// commit 成功後の panic を「サイレントな接続断」ではなく同期的な ErrorResponse
/// として観測でき（RECOVER-6 が防ぐ範囲）、`D` フィールドにより「commit は
/// 成功しているかもしれない」という状態情報も併せて受け取れる（ERR-5・
/// 2026-09-14 確定・`vector-db-spec#15`。`crate::error_response` モジュール
/// ドキュメント参照）。
///
/// `internal_error` は呼び出し元が構築済みの `WireError::internal()` を渡す契約
/// （通常経路の内部エラー応答と同じ固定文言・`wire_code` を使い、文言を二重に
/// 持たない）。
///
/// **本関数はバイト列を組み立てるだけで、送信するかどうかには一切関与しない**
/// （codex-review P1 指摘対応・PR #258）。この事前エンコード済みバイト列は
/// [`cached_emergency_response_bytes`] を経由して `execute_and_respond` の
/// 「outcome を決定する区間」（`engine.execute_sql_in_session` 呼び出しを
/// 含むブロック）の**開始前**に登録されるが、実際に panic フックが
/// これをソケットへ書き込む（送出する）かどうかは
/// `engine::recovery::panic_hook::emergency_send_decision` が別途判定する。
/// 同関数は「このスレッドが commit 成功後・応答未確定の区間にあるか
/// （`engine::recovery::commit_boundary::active_commit_pending_generation` が
/// `Some`）」かつ「その世代が登録時に捕捉した世代と一致するか」の両方を
/// 満たす場合にのみ真を返す。`execute_sql_in_session` の読み取り専用 5 分岐
/// （`SetSearchMode`・`CreateFunction`・`Select`・`Aggregate`・`Explain`）はいずれも
/// `engine::recovery::commit_boundary::commit`／`commit_and_finish` を呼ばない
/// ため、これらの区間で panic しても commit-pending 世代は立たず
/// `emergency_send_decision` は偽となる ―― 登録は存在するが送出されない
/// （前段フック `previous_hook` へ委譲され、TASK-97 以前と同じ「接続断のみ」に
/// 倒れる）。一方 `Insert`（TASK-82・SQL-10）は `execute_insert_sql` 経由で
/// `commit_boundary::commit` を実際に呼ぶ commit を伴う分岐であり、この区間で
/// 該当世代の commit が成功した後に panic すれば `emergency_send_decision` は
/// 真となり緊急応答が送出される（`engine::recovery::panic_hook` モジュール
/// ドキュメント・`engine/tests/recover6_panic_hook.rs` の
/// `EngineCore::insert_row` 経由の検証がこの commit 境界機構自体の契約を
/// 既に固定している。本ファイルはその機構を SQL 表層の `INSERT` 経路へ
/// 接続するのみで、機構自体を新設しない）。すなわち「事前に登録される」ことと
/// 「実際に送出される」ことは別軸であり、送出可否の唯一の判断材料は panic
/// 発生時点の commit 成功フラグ（世代一致）である。回帰テストは
/// `engine::recovery::panic_hook::tests::
/// try_send_emergency_response_returns_false_when_not_pending_even_if_registered`
/// を参照。
fn build_emergency_response_bytes(
    internal_error: &engine::error_format::WireError,
) -> Result<Vec<u8>, result_encoder::EncodeError> {
    crate::error_response::encode_with_detail(
        internal_error.class(),
        internal_error.message(),
        crate::error_response::MAY_BE_COMMITTED_DETAIL,
    )
}

/// [`build_emergency_response_bytes`] の結果をプロセス生存期間でキャッシュする
/// （TASK-97・RECOVER-6、codex-review Medium 指摘対応・PR #90）。
///
/// `WireError::internal()` の固定文言のみに依存し、クエリごとに内容が変わらない
/// ため、初回呼び出し時に一度だけ構築する。以降の呼び出しは `OnceLock` の読み取り
/// のみで、`WireError::internal()` の構築・エンコード・アロケーションを毎クエリ
/// 発生させない。エンコード自体が失敗した場合（通常発生しない想定）は `None` を
/// キャッシュし、以降も緊急応答チャネルへ登録しない側（fail-closed）に倒れる。
fn cached_emergency_response_bytes() -> Option<&'static Vec<u8>> {
    static CACHE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let internal_error = engine::error_format::WireError::internal();
            build_emergency_response_bytes(&internal_error).ok()
        })
        .as_ref()
}

/// [`cached_emergency_response_bytes`] の薄い公開ラッパー（TASK-97・ERR-5）。
///
/// wire-server が実際に組み立てる緊急応答バイト列（`D`=`state=may_be_committed`
/// 込み）を crate 外から取得するための唯一の公開経路。呼び出し文脈は
/// `crates/wire-server/tests/wire_emergency_response.rs`（層 A 結合テスト）
/// ―― engine 側の `engine::recovery::panic_hook::EmergencyResponseRegistration::
/// register` へ登録するバイト列として、`execute_and_respond` の実運用経路と
/// 同一のエンコード結果を渡すために使う。本関数自体は登録・送出のいずれにも
/// 関与しない（`build_emergency_response_bytes` のドキュメント参照）。
pub fn emergency_response_bytes() -> Option<&'static [u8]> {
    cached_emergency_response_bytes().map(Vec::as_slice)
}

/// 検索 SELECT（`command_tag` = `"SELECT"`）・`EXPLAIN`（`command_tag` =
/// `"EXPLAIN"`。TASK-78・SQL-6）いずれの応答整形にも使う共通経路。行の
/// `RowDescription`/`DataRow` エンコードは両者で共通（`ColumnMeta`/`ResultRow`
/// の汎用性による）。`EXPLAIN` の CommandComplete タグは pg 互換で行数を
/// 付けない（`"EXPLAIN"` 固定。検索 SELECT は既存どおり `"SELECT <行数>"`）。
///
/// Issue #481: 以前は `RowDescription`・各 `DataRow`・`CommandComplete`・
/// `ReadyForQuery` をそれぞれ個別の `write_all`（行数 + 3 回のシステムコール）
/// で送出しており、`docs/design/knn-wire-stage-profile.md`「スコープ外・
/// 申し送り」でこの点が先送りされていた。`crate::response_buffer::
/// ResponseBuffer` へ全フレームを積み、上限
/// （`crate::limits::MAX_RESPONSE_BUFFER_BYTES`）を超えない限り最後に一括
/// `flush` することで、応答一式が上限以下に収まる大半のケースでは
/// 1 回の `write_all` になる。上限超過時はフレーム境界で分割送出する
/// （`ResponseBuffer` のドキュメント参照。「拒否」ではなくバッファ有界化の
/// ためのフラッシュ閾値）。
///
/// `_response_boundary`（RECOVER-5 (3)）・`_emergency_registration`
/// （RECOVER-6）との関係: 本関数はいずれも `run_statement` が
/// 「outcome を決定する区間」を抜けた後（`_emergency_registration` の
/// スコープ外）に呼ばれ、`_response_boundary` の生存区間内で完結する。
/// バッファ組み立て自体はメモリ上の操作でしかなく、commit 成功境界・
/// 応答一意性の契約（`_response_boundary`）には影響しない。
///
/// WIRE-16: `finish` が [`Finish::Continue`] のときは `ReadyForQuery` を送らず
/// 次の文へ制御を返す（[`StatementStatus::Completed`]）。エラーに切り替わる
/// 経路は `finish` に関係なく常に ErrorResponse＋`ReadyForQuery` を送り
/// [`StatementStatus::Failed`] を返す（途中エラーで打ち切るため、その時点で
/// 応答を確定する）。
///
/// Issue #934: `run_statement` は `map_outcome`／`TagShape` 経由で直接
/// `respond_rows_with_tag` を呼ぶようになったため、本関数は production 経路では
/// 使われなくなった（`TagShape` が `SELECT`/`EXPLAIN` のタグ組み立てを一元化した
/// ため）。バイト列契約の回帰テスト（`respond_query_result_matches_*`）専用の
/// ヘルパーとして残す。
#[cfg(test)]
fn respond_query_result<S: WireStream>(
    stream: &mut S,
    result: &engine::sql::exec::QueryResult,
    command_tag: &str,
    finish: Finish,
) -> io::Result<StatementStatus> {
    let tag = if command_tag == "EXPLAIN" {
        command_tag.to_string()
    } else {
        format!("{command_tag} {}", result.rows.len())
    };
    // テスト専用ヘルパーのため明示トランザクションは持たない（`Idle` のまま）。
    // `respond_rows_with_tag` は `Active` 以外では `fail()` が no-op になるため、
    // 既存のバイト列契約（`ReadyForQuery` は常に `Idle`）は変わらない。
    let mut txn = SessionTransaction::new(engine::sql::transaction::TransactionLimits::default());
    respond_rows_with_tag(stream, result, &tag, finish, &mut txn)
}

/// `RowDescription`／`DataRow`* の組み立てとフレーム送出を担う共通本体
/// （Issue #873・SQL-21 で [`respond_query_result`] から切り出した）。`tag` は
/// 呼び出し元が完成済みで渡す `CommandComplete` の中身（`SELECT`／`EXPLAIN` は
/// `respond_query_result` が `result.rows.len()` から組み立て、`RETURNING` は
/// `outcome.rows_affected` から組み立てる。`run_statement` の
/// `SqlOutcome::Returning` 分岐参照）。バイト列の組み立て自体（`ResponseBuffer`
/// によるバッファリング・上限超過時のフレーム境界分割送出）は本切り出しの
/// 前後で完全に同一。`finish`／戻り値の意味は [`respond_query_result`] 参照。
fn respond_rows_with_tag<S: WireStream>(
    stream: &mut S,
    result: &engine::sql::exec::QueryResult,
    tag: &str,
    finish: Finish,
    txn: &mut SessionTransaction<'_>,
) -> io::Result<StatementStatus> {
    let row_desc = match result_encoder::encode_row_description(&result.columns) {
        Ok(msg) => msg,
        Err(_) => {
            // エンコード失敗は engine 側の実行結果とは独立に発生しうる wire 層
            // 固有の失敗だが、SQL-31 の契約（`Active` 中のエラーは種類を問わず
            // `Failed` へ遷移させる）は応答生成の失敗にも及ぶ（PR #1041 レビュー
            // 指摘 P1）。ここで `fail()` せずに `Active` のまま返すと、直前の
            // `INSERT` 等の書き込みが未確定のまま残り、後続の `COMMIT` で
            // 誤って永続化されてしまう。
            txn.fail();
            respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode row description",
                txn.status(),
            )?;
            return Ok(StatementStatus::Failed);
        }
    };

    // 初期確保は上限（`MAX_RESPONSE_BUFFER_BYTES`）を超えない範囲での概算
    // ヒント（`RowDescription` 長 + 行数 × 64 バイト目安）に留める（untrusted
    // 入力に基づく無制限確保を避ける規約に従う。実サイズが見積りを超えても
    // `Vec` は必要に応じて再確保するだけで、上限超過時は `push_frame` が
    // 途中で `flush` する）。
    let hint = row_desc
        .len()
        .saturating_add(result.rows.len().saturating_mul(64));
    let mut buffer = crate::response_buffer::ResponseBuffer::with_capacity_hint(
        crate::limits::MAX_RESPONSE_BUFFER_BYTES,
        hint,
    );
    buffer.push_frame(stream, &row_desc)?;

    for row in &result.rows {
        let start = buffer.frame_start();
        match result_encoder::encode_data_row_into(row, buffer.as_mut_vec()) {
            Ok(()) => {}
            Err(_) => {
                // 失敗時は `encode_data_row_into` 自身が書きかけを巻き戻し
                // 済みだが、念のため呼び出し側でも同じ位置まで truncate する
                // （in-place エンコードの契約: `start` の直後から追記する前提が
                // 崩れた場合の防御）。完成済みフレームは先に送出してから
                // ErrorResponse へ切り替える（部分フレームを絶対に残さない）。
                buffer.truncate_to(start);
                buffer.flush(stream)?;
                // 行エンコード失敗も上記と同じ理由で `Active` を `Failed` へ
                // 強制遷移させる（SQL-31・PR #1041 レビュー指摘 P1）。
                txn.fail();
                respond_error_and_ready(
                    stream,
                    ErrorClass::InternalError,
                    "failed to encode data row",
                    txn.status(),
                )?;
                return Ok(StatementStatus::Failed);
            }
        }
        if buffer.len() >= crate::limits::MAX_RESPONSE_BUFFER_BYTES {
            buffer.flush(stream)?;
        }
    }

    match result_encoder::encode_command_complete(tag) {
        Ok(msg) => {
            buffer.push_frame(stream, &msg)?;
            if finish == Finish::ReadyForQuery {
                buffer.push_frame(
                    stream,
                    &result_encoder::encode_ready_for_query(txn.status()),
                )?;
            }
            buffer.flush(stream)?;
            Ok(StatementStatus::Completed)
        }
        Err(_) => {
            buffer.flush(stream)?;
            // `CommandComplete` エンコード失敗も同様に `Failed` へ遷移させる
            // （SQL-31・PR #1041 レビュー指摘 P1）。
            txn.fail();
            respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode command complete response",
                txn.status(),
            )?;
            Ok(StatementStatus::Failed)
        }
    }
}

// engine クレートの一時 DB ヘルパーをソース取り込みで共有する（wire-server の
// 結合テスト `tests/*.rs` と同じ方式。`crates/engine/src/test_util/temp_db.rs`
// モジュールドキュメント参照）。`#[path]` はこのファイル（`src/simple_query.rs`）
// 自身の位置基準で解決されるため、ネストした `mod tests` の内側ではなく
// トップレベルで宣言する（結合テストの `tests/*.rs` と同じ相対深さ）。
#[cfg(test)]
#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::ColumnType;
    use engine::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
    use std::net::{TcpListener, TcpStream};

    /// 実ソケットを介したループバック対（`protocol_dispatch.rs::tests::
    /// loopback_pair` と同じパターン）。大容量応答の送信ブロックを避けるため、
    /// 呼び出し元は必ず受信を別スレッドで並行実行すること（送信バッファの
    /// 詰まりによるデッドロックを避ける）。
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    fn read_exact_owned(stream: &mut TcpStream, len: usize) -> Vec<u8> {
        use std::io::Read as _;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).expect("read_exact");
        buf
    }

    /// `respond_query_result` が組み立てるバイト列は、個別エンコード
    /// （`RowDescription` + 各 `DataRow` + `CommandComplete` + `ReadyForQuery`
    /// をそれぞれ `Vec<u8>` として結合したもの）と完全に一致すること
    /// （Issue #481: 送出回数を変えても内容は不変という契約の固定）。
    #[test]
    fn respond_query_result_matches_individually_encoded_concatenation() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: ColumnType::Text,
            },
        ];
        let rows: Vec<ResultRow> = (0..50)
            .map(|i| ResultRow {
                id: i,
                score: 0.0,
                cells: vec![Cell::Integer(i), Cell::Text(format!("row-{i}"))],
            })
            .collect();
        let result = QueryResult {
            columns: columns.clone(),
            rows: rows.clone(),
        };

        let mut expected = result_encoder::encode_row_description(&columns).expect("row desc");
        for row in &rows {
            expected.extend_from_slice(&result_encoder::encode_data_row(row).expect("data row"));
        }
        expected.extend_from_slice(
            &result_encoder::encode_command_complete(&format!("SELECT {}", rows.len()))
                .expect("command complete"),
        );
        expected.extend_from_slice(&result_encoder::encode_ready_for_query(
            engine::sql::transaction::TransactionStatus::Idle,
        ));

        let (mut server, mut client) = loopback_pair();
        let expected_len = expected.len();
        let reader = std::thread::spawn(move || read_exact_owned(&mut client, expected_len));
        respond_query_result(&mut server, &result, "SELECT", Finish::ReadyForQuery)
            .expect("respond");
        let received = reader.join().expect("reader thread");

        assert_eq!(received, expected);
    }

    /// 行の途中でエンコード不能な行（`i16` に収まらないセル数）が混在する場合、
    /// 完成済みフレーム（`RowDescription` + 先行 `DataRow`）を送出してから
    /// `ErrorResponse`（`XX000`）+ `ReadyForQuery` へ切り替わり、部分フレームが
    /// 混入しないこと（Issue #481: `ResponseBuffer`/`encode_data_row_into` の
    /// 巻き戻し契約の end-to-end 確認）。
    #[test]
    fn respond_query_result_flushes_completed_rows_then_errors_on_unencodable_row() {
        let columns = vec![ColumnMeta::Id];
        let good_row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Integer(1)],
        };
        // i16::MAX + 1 セルは `encode_data_row_into` を必ず失敗させる。
        let bad_row = ResultRow {
            id: 2,
            score: 0.0,
            cells: vec![Cell::Null; 32_768],
        };
        let result = QueryResult {
            columns: columns.clone(),
            rows: vec![good_row.clone(), bad_row],
        };

        let mut expected_prefix =
            result_encoder::encode_row_description(&columns).expect("row desc");
        expected_prefix
            .extend_from_slice(&result_encoder::encode_data_row(&good_row).expect("data row"));

        let (mut server, mut client) = loopback_pair();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).expect("read_to_end");
            buf
        });
        respond_query_result(&mut server, &result, "SELECT", Finish::ReadyForQuery)
            .expect("respond");
        drop(server);
        let received = reader.join().expect("reader thread");

        assert!(
            received.starts_with(&expected_prefix),
            "completed RowDescription + first DataRow must be flushed before the error"
        );
        let tail = &received[expected_prefix.len()..];
        assert_eq!(
            tail.first().copied(),
            Some(b'E'),
            "must switch to ErrorResponse"
        );
        assert!(
            tail.windows(5).any(|w| w == b"XX000"),
            "internal encode failure must surface as XX000"
        );
        assert_eq!(
            tail.last().copied(),
            Some(b'I'),
            "must still send ReadyForQuery after the error"
        );
    }

    /// SQL-31（PR #1041 レビュー指摘 P1）: 明示 `BEGIN` で `Active` 中に
    /// `respond_rows_with_tag` が応答エンコードへ失敗した場合、`ErrorResponse` を
    /// 返すだけでなく `txn` 自身を `Failed` へ強制遷移させること。遷移させないと
    /// クライアントにはエラーが返る一方で、先行する書き込みが後続の `COMMIT` で
    /// 誤って確定してしまう（`respond_error_and_ready` の `ReadyForQuery` にも
    /// 遷移後の `Failed`＝`'E'` が反映されることまで固定する）。
    #[test]
    fn respond_rows_with_tag_fails_active_transaction_on_encode_error() {
        let path = temp_db::unique_db_path("respond-rows-with-tag-active-txn-fail");
        let _guard = temp_db::CleanupGuard(path.clone());
        let storage = engine::storage::Storage::open(&path).expect("open storage");

        let mut txn =
            SessionTransaction::new(engine::sql::transaction::TransactionLimits::default());
        txn.begin(&storage, &SessionState::default())
            .expect("begin explicit transaction");
        assert_eq!(
            txn.status(),
            engine::sql::transaction::TransactionStatus::InTransaction,
            "precondition: txn must be Active before the encode failure"
        );

        // `encode_data_row_into` を必ず失敗させる不正な行（`i16::MAX` を超える
        // セル数）。
        let columns = vec![ColumnMeta::Id];
        let bad_row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null; 32_768],
        };
        let result = QueryResult {
            columns,
            rows: vec![bad_row],
        };

        let (mut server, mut client) = loopback_pair();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).expect("read_to_end");
            buf
        });
        let status = respond_rows_with_tag(
            &mut server,
            &result,
            "SELECT 1",
            Finish::ReadyForQuery,
            &mut txn,
        )
        .expect("respond");
        drop(server);
        let received = reader.join().expect("reader thread");

        assert_eq!(status, StatementStatus::Failed);
        assert_eq!(
            txn.status(),
            engine::sql::transaction::TransactionStatus::Failed,
            "encode failure during Active must force-transition the transaction to Failed \
             (SQL-31 abort-on-error contract)"
        );
        assert_eq!(
            received.last().copied(),
            Some(b'E'),
            "ReadyForQuery must reflect the post-transition Failed status ('E'), not the \
             pre-failure Active status"
        );
    }

    /// SQL-31・TASK-221（PR #1041 レビュー指摘）: `CommandComplete` 単独応答の
    /// エンコードが失敗した場合も、`Active` な明示トランザクションを `Failed` へ
    /// 強制遷移させること（`Active` のまま残すと、クライアントにはエラーが返る
    /// 一方で直前の `INSERT`／`TRUNCATE` が後続の `COMMIT` で確定してしまう）。
    /// `ReadyForQuery` にも遷移後の `Failed`（`'E'`）が反映されることまで固定する。
    #[test]
    fn respond_command_complete_fails_active_transaction_on_encode_error() {
        let path = temp_db::unique_db_path("respond-command-complete-active-txn-fail");
        let _guard = temp_db::CleanupGuard(path.clone());
        let storage = engine::storage::Storage::open(&path).expect("open storage");

        let mut txn =
            SessionTransaction::new(engine::sql::transaction::TransactionLimits::default());
        txn.begin(&storage, &SessionState::default())
            .expect("begin explicit transaction");
        assert_eq!(
            txn.status(),
            engine::sql::transaction::TransactionStatus::InTransaction,
            "precondition: txn must be Active before the encode failure"
        );

        let (mut server, mut client) = loopback_pair();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).expect("read_to_end");
            buf
        });
        let status = respond_command_complete_with(
            &mut server,
            "INSERT 0 1",
            Finish::ReadyForQuery,
            &mut txn,
            |_| Err(result_encoder::EncodeError),
        )
        .expect("respond");
        drop(server);
        let received = reader.join().expect("reader thread");

        assert_eq!(status, StatementStatus::Failed);
        assert_eq!(
            txn.status(),
            engine::sql::transaction::TransactionStatus::Failed,
            "encode failure during Active must force-transition the transaction to Failed"
        );
        assert_eq!(received.first().copied(), Some(b'E'), "ErrorResponse first");
        assert_eq!(
            received.last().copied(),
            Some(b'E'),
            "ReadyForQuery must reflect the post-transition Failed status ('E')"
        );
    }

    /// 対照: エンコードが成功すれば `Active` のまま `CommandComplete` を返す
    /// （上のテストの遷移判定が空振りしていないことの確認）。
    #[test]
    fn respond_command_complete_keeps_active_transaction_on_success() {
        let path = temp_db::unique_db_path("respond-command-complete-active-txn-ok");
        let _guard = temp_db::CleanupGuard(path.clone());
        let storage = engine::storage::Storage::open(&path).expect("open storage");
        let mut txn =
            SessionTransaction::new(engine::sql::transaction::TransactionLimits::default());
        txn.begin(&storage, &SessionState::default())
            .expect("begin explicit transaction");

        let (mut server, mut client) = loopback_pair();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).expect("read_to_end");
            buf
        });
        let status =
            respond_command_complete(&mut server, "INSERT 0 1", Finish::ReadyForQuery, &mut txn)
                .expect("respond");
        drop(server);
        let received = reader.join().expect("reader thread");

        assert_eq!(status, StatementStatus::Completed);
        assert_eq!(
            txn.status(),
            engine::sql::transaction::TransactionStatus::InTransaction
        );
        assert_eq!(received.first().copied(), Some(b'C'));
        assert_eq!(received.last().copied(), Some(b'T'));
    }

    /// 複数文メッセージのテスト用に、応答を読み捨てるループバック対と engine を
    /// 用意して `execute_and_respond` を順に実行する。
    fn run_messages(
        engine: &EngineCore,
        session: &mut SessionState,
        messages: &[&str],
    ) -> engine::sql::transaction::TransactionStatus {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");
        let mut txn = engine.new_session_transaction();
        let (mut server, mut client) = loopback_pair();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).expect("read_to_end");
            buf
        });
        for sql in messages {
            execute_and_respond(&mut server, engine, &ctx, session, &mut txn, sql)
                .expect("execute_and_respond");
        }
        let status = txn.status();
        drop(txn);
        drop(server);
        let _ = reader.join().expect("reader thread");
        status
    }

    fn new_engine(label: &str) -> (EngineCore, temp_db::CleanupGuard) {
        let path = temp_db::unique_db_path(label);
        let guard = temp_db::CleanupGuard(path.clone());
        let storage = engine::storage::Storage::open(&path).expect("open storage");
        (
            EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider)),
            guard,
        )
    }

    /// SQL-31・TASK-221（PR #1041 レビュー指摘）: 同じメッセージ内の `ROLLBACK`
    /// が `BEGIN` 時点の状態へ戻した後に後続文が失敗しても、メッセージ受信前
    /// （トランザクション中）の `SET` を復活させないこと。
    #[test]
    fn failure_after_rollback_in_same_message_keeps_rolled_back_session_state() {
        use engine::sql::mode::SearchMode;
        let (engine, _guard) = new_engine("simple-query-rollback-then-failure");
        let mut session = SessionState::default();
        let status = run_messages(
            &engine,
            &mut session,
            &[
                "SET search_mode = 'precision'",
                "BEGIN",
                "SET search_mode = 'recall'",
                "ROLLBACK; SELEC 1",
            ],
        );
        assert_eq!(status, engine::sql::transaction::TransactionStatus::Idle);
        assert_eq!(
            session.search_mode(),
            Some(SearchMode::Precision),
            "a failure after ROLLBACK must not resurrect the in-transaction SET"
        );
    }

    /// 同じメッセージ内で `BEGIN` した後の文が失敗した場合、巻き戻し先は `BEGIN`
    /// の時点（＝`ROLLBACK` が復元する状態と同じ）であること。
    #[test]
    fn failure_after_begin_in_same_message_restores_to_begin_boundary() {
        use engine::sql::mode::SearchMode;
        let (engine, _guard) = new_engine("simple-query-begin-then-failure");
        let mut session = SessionState::default();
        let status = run_messages(
            &engine,
            &mut session,
            &["SET search_mode = 'precision'; BEGIN; SET search_mode = 'recall'; SELEC 1"],
        );
        assert_eq!(status, engine::sql::transaction::TransactionStatus::Failed);
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));

        let status = run_messages(&engine, &mut session, &["ROLLBACK"]);
        assert_eq!(status, engine::sql::transaction::TransactionStatus::Idle);
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));
    }

    /// 対照: トランザクション境界を跨がない複数文メッセージの途中失敗は、従来
    /// どおりメッセージ受信前の状態へ巻き戻すこと。
    #[test]
    fn failure_without_transaction_boundary_restores_pre_message_state() {
        use engine::sql::mode::SearchMode;
        let (engine, _guard) = new_engine("simple-query-plain-failure");
        let mut session = SessionState::default();
        run_messages(
            &engine,
            &mut session,
            &[
                "SET search_mode = 'precision'",
                "SET search_mode = 'recall'; SELEC 1",
            ],
        );
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));
    }

    /// 失敗した `COMMIT`（`Active` → `Idle`。`BEGIN` 時点の状態を復元済み）の後は、
    /// メッセージ内スナップショットでその復元結果を上書きしないこと（commit 自体の
    /// 失敗は wire 層から注入できないため、判定部分を直接検証する。engine 側の
    /// 復元は `sql::transaction` の `commit_failure_restores_session_state_at_begin_
    /// and_returns_to_idle` が固定する）。
    #[test]
    fn message_snapshot_does_not_overwrite_failed_commit_restore() {
        use engine::sql::mode::SearchMode;
        use engine::sql::transaction::TransactionStatus;
        let (engine, _guard) = new_engine("simple-query-snapshot-failed-commit");
        let mut in_txn = SessionState::default();
        run_messages(&engine, &mut in_txn, &["SET search_mode = 'recall'"]);
        let mut at_begin = SessionState::default();
        run_messages(&engine, &mut at_begin, &["SET search_mode = 'precision'"]);

        // メッセージ受信時点（トランザクション中）の状態を保持したスナップショット。
        let snapshot = MessageSnapshot::new(&in_txn);
        // 失敗した COMMIT が `session` を BEGIN 時点へ復元済みの状態。
        let mut session = at_begin.clone();
        snapshot.restore_after_failure(
            TransactionStatus::InTransaction,
            TransactionStatus::Idle,
            &mut session,
        );
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));

        // 対照: トランザクションを閉じない失敗（Active → Failed）は巻き戻す。
        let snapshot = MessageSnapshot::new(&in_txn);
        let mut session = at_begin;
        snapshot.restore_after_failure(
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            &mut session,
        );
        assert_eq!(session.search_mode(), Some(SearchMode::Recall));
    }

    /// ERR-5: [`emergency_response_bytes`] が返すバイト列（`crates/wire-server/
    /// tests/wire_emergency_response.rs` が層 A で送受信するのと同じキャッシュ
    /// 済み実体）を直接パースし、`S`=`ERROR`・`C`=`XX000`・`M`=`internal error`・
    /// `D` がちょうど 1 個で [`crate::error_response::MAY_BE_COMMITTED_DETAIL`]
    /// と一致することを固定する（サブプロセステストが環境要因で flaky になった
    /// 場合にも残る最小の固定点）。
    #[test]
    fn emergency_response_bytes_carries_may_be_committed_detail() {
        let bytes = emergency_response_bytes().expect("emergency response bytes must encode");
        assert_eq!(bytes.first().copied(), Some(b'E'), "type byte");

        let declared_len = i32::from_be_bytes(
            bytes
                .get(1..5)
                .expect("length field")
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        assert_eq!(
            declared_len,
            bytes.len() - 1,
            "length field excludes only the leading 'E' type byte"
        );

        let body = bytes.get(5..).expect("body");

        // フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出するテスト
        // 専用ヘルパー。受信データ経路ではないため `unwrap`/`expect` は許容する
        // （`.claude/rules/coding-rust.md` の添字アクセス禁止は untrusted 受信
        // 入力経路が対象。`error_response.rs::tests::find_field` と同型）。
        fn find_field(body: &[u8], tag: u8) -> Option<String> {
            let mut idx = 0;
            while idx < body.len() {
                let this_tag = *body.get(idx)?;
                if this_tag == 0 {
                    return None;
                }
                let value_start = idx + 1;
                let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
                let value_end = value_start + nul_offset;
                if this_tag == tag {
                    let field_bytes = body.get(value_start..value_end)?;
                    return std::str::from_utf8(field_bytes).ok().map(str::to_string);
                }
                idx = value_end + 1;
            }
            None
        }

        assert_eq!(find_field(body, b'S').as_deref(), Some("ERROR"));
        assert_eq!(find_field(body, b'C').as_deref(), Some("XX000"));
        assert_eq!(find_field(body, b'M').as_deref(), Some("internal error"));
        assert_eq!(
            find_field(body, b'D').as_deref(),
            Some(crate::error_response::MAY_BE_COMMITTED_DETAIL)
        );
        assert_eq!(
            body.iter().filter(|&&b| b == b'D').count(),
            1,
            "D field must appear exactly once"
        );
        assert_eq!(body.last().copied(), Some(0), "field terminator");
    }
}
