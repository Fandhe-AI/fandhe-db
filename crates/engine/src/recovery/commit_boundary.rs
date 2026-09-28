//! TASK-96（対象ビヘイビア: RECOVER-5）の commit 成功境界実装。
//!
//! `redb` への commit 成功を書き込み操作の「もう後戻りできない地点」
//! （point of no return）と定め、書き込み実行中の失敗タイミングを問わず
//! 1 回の呼び出しに対する応答が常に一意（二重応答なし）であることを構造的に
//! 保証する choke point を提供する。呼び出し元は `crate::tenant`（各
//! `*_unchecked` 関数）・`crate::txn`（`WriteTxn`/`BatchWriteTxn` の commit
//! 一本化点、[`commit_write_txn_guarded`] 経由）に加え、`crate::storage`
//! （`Storage::put`/`put_batch`）・`crate::catalog`（テーブル DDL・
//! `insert_row_into_table` 系の DML）が [`commit`] を直接呼ぶ経路も含む
//! （本クレート内で実書き込みを伴う commit（`write_txn.commit()` を到達させる
//! 経路）はすべて本モジュールの choke point を経由し、`crate::storage::
//! bump_generation_and_commit` を直接呼ばない）。これらはいずれも wire 層の
//! DML 入口（`crate::core::EngineCore`）が最終的に通る経路。
//!
//! 本モジュールが提供する型: [`PostCommitResult`]・[`PostCommitPanicGuard`]・
//! [`ResponseBoundaryGuard`]。各型が担う保護区間・契約の詳細はそれぞれの
//! ドキュメンテーションコメントを参照（対象ビヘイビア: RECOVER-5）。

use std::cell::Cell;

use crate::storage::{self, Result as StorageResult};

thread_local! {
    /// このスレッドで現在アクティブな [`ResponseBoundaryGuard`] の世代番号
    /// （RECOVER-5。0 は「アクティブなガードなし」を表す予約値）。
    /// [`ResponseBoundaryGuard::new`] が生成時に採番・記録し、drop 時に
    /// 自分の世代と一致していればクリアする。[`commit_and_finish`] は commit
    /// 成功時点でここに記録されている世代（0 でなければ）だけを
    /// [`COMMIT_PENDING_RESPONSE`] へ引き渡す対象とする。
    static ACTIVE_RESPONSE_BOUNDARY_GENERATION: Cell<u64> = const { Cell::new(0) };

    /// 次に払い出す [`ResponseBoundaryGuard`] の世代番号。`saturating_add` で
    /// 増分するため u64 を使い切らない限りオーバーフローしない
    /// （coding-rust.md「整数演算は checked_*/saturating_* を使う」）。
    static NEXT_RESPONSE_BOUNDARY_GENERATION: Cell<u64> = const { Cell::new(1) };

    /// commit 成功後、wire 層の応答確定（[`ResponseBoundaryGuard`] の drop）が
    /// まだ済んでいない区間かどうかを表すスレッドローカルフラグ（RECOVER-5）。
    /// [`commit_and_finish`] が commit 成功時にアクティブなガードが存在する場合、
    /// そのガードの世代番号を記録する（アクティブなガードがなければ何もしない
    /// ―― stale フラグによる誤 abort を防ぐための契約。[`ResponseBoundaryGuard`]
    /// のドキュメント参照）。[`ResponseBoundaryGuard`] の drop 時に読み取ってクリアする。
    static COMMIT_PENDING_RESPONSE: Cell<Option<u64>> = const { Cell::new(None) };
}

/// commit 成功から wire 層の応答確定までの区間全体を覆う RAII ガード
/// （RECOVER-5。[`PostCommitPanicGuard`] より広い区間を守る）。
///
/// `wire-server::simple_query::execute_and_respond` が 1 クエリの処理開始時に
/// 生成し、応答をすべて書き終えるまで所有する契約。現在の唯一の呼び出し元は
/// ネストせず 1 クエリにつき 1 個のみ生成するが、
/// 本型は `pub`（wire-server から呼ばれる）で将来の呼び出し元がこの契約を破って
/// ネストして生成する可能性を構造的に排除できない。そのため、ネストしても
/// 外側の保護区間が壊れない「非所有」設計にしている（codex-review P1 再指摘・
/// PR #246 #discussion_r3873683862 対応。「想定していない」だけでは構造的保証に
/// ならないという指摘に対し、ネストされても安全な構造そのもので応える）。
///
/// - 生成時、スレッドに既にアクティブなガード（[`ACTIVE_RESPONSE_BOUNDARY_GENERATION`]
///   が非 0）が存在しなければ、新しい世代番号を採番して「境界を所有する」ガードと
///   なる（`owned_generation = Some(generation)`）。既にアクティブなガードが存在
///   する場合（＝ネストして生成された内側のガード）は境界を所有せず
///   （`owned_generation = None`）、[`ACTIVE_RESPONSE_BOUNDARY_GENERATION`] を
///   一切書き換えない。これにより外側ガードの世代がスレッドローカルへ残り続け、
///   [`commit_and_finish`] が内側の区間内で commit してもその世代（＝外側の世代）
///   が [`COMMIT_PENDING_RESPONSE`] へ記録される。
/// - `owned_generation == None`（非所有＝内側ガード）の drop は何もしない
///   （abort 判定もスレッドローカルの書き換えも行わない）。外側ガードの保護
///   区間・pending フラグを一切触らないため、内側ガードの生成・commit・
///   正常 drop のいずれの組み合わせでも外側の保護は失われない。
/// - `owned_generation == Some(generation)`（所有＝外側 or 単独ガード）の drop
///   は、記録されている pending 世代が自分自身の世代と一致する場合のみクリアし
///   「commit 成功後まだ応答未確定」と判定する。一致しない場合は pending を
///   一切変更しない（他ガードの記録を誤って消さない）。
///
/// `must_use` 属性は束縛忘れ（`let _ = ResponseBoundaryGuard::new();` 等で即座に
/// drop され保護区間が消える）をコンパイル時の warning として検出するための注記。
/// 実際に使う際は名前付きの変数（例: `let _response_boundary = ...;`）で応答確定
/// まで保持すること。
#[must_use]
pub struct ResponseBoundaryGuard {
    /// 境界を所有する場合のみ `Some(自世代)`。ネストして生成された内側の
    /// ガードは `None`（＝ drop 時に何もしない）。
    owned_generation: Option<u64>,
}

impl ResponseBoundaryGuard {
    /// クエリ処理の入口で呼ぶ。応答をすべて書き終えるまでこの戻り値を
    /// 名前付き変数へ束縛して生存させること（`let _ = ...` は即座に drop され
    /// 保護区間が失われるため使わない）。
    pub fn new() -> Self {
        let owned_generation = ACTIVE_RESPONSE_BOUNDARY_GENERATION.with(|c| {
            if c.get() != 0 {
                // 既にアクティブなガードが存在する＝ネストして生成された。
                // 境界の所有権は外側ガードに残す（構造体ドキュメント参照）。
                None
            } else {
                let generation = NEXT_RESPONSE_BOUNDARY_GENERATION.with(|n| {
                    let current = n.get();
                    n.set(current.saturating_add(1));
                    current
                });
                c.set(generation);
                Some(generation)
            }
        });
        Self { owned_generation }
    }
}

impl Default for ResponseBoundaryGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ResponseBoundaryGuard {
    fn drop(&mut self) {
        // 境界を所有していない（＝ネストして生成された内側の）ガードは、
        // 外側の保護区間・pending フラグに一切触れずに何もしない
        // （構造体ドキュメント参照）。
        let Some(generation) = self.owned_generation else {
            return;
        };

        // 記録されている pending 世代が自分自身のものと一致する場合のみ abort
        // 対象とみなし、その場合のみクリアする。一致しない場合は pending を
        // 変更しない（他ガードの記録を誤って消さないため）。
        let armed = COMMIT_PENDING_RESPONSE.with(|f| {
            if f.get() == Some(generation) {
                f.set(None);
                true
            } else {
                false
            }
        });

        // アクティブ世代の記録も自分自身のものであればクリアする。
        ACTIVE_RESPONSE_BOUNDARY_GENERATION.with(|c| {
            if c.get() == generation {
                c.set(0);
            }
        });

        if should_abort(armed, std::thread::panicking()) {
            // 固定の英語文言のみを stderr へ出す。テナント ID・テーブル名・行データ・
            // operation_id を一切含めない（security.md P0「エラー・ログ経由で他テナントの
            // データ・存在情報を漏らさない」）。
            crate::log_stderr!(
                "fatal: panic occurred after a write transaction committed successfully; \
                 aborting the process to avoid returning a false success response"
            );
            std::process::abort();
        }
    }
}

/// commit 成功後の派生状態（索引等）反映の結果。反映が失敗しても、確定済みの
/// 成功応答（commit 自体は成功している）を `Err` へ転化させない（RECOVER-5）。
/// 現状 `crate::tenant`・`crate::txn` の書き込み経路には
/// commit 後の同期的な索引反映が存在しないため、既存呼び出し元はすべて
/// [`PostCommitResult::Ok`] のみを渡す。将来 commit 後の派生状態反映を追加する
/// 際の受け皿として型を用意しておく。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostCommitResult {
    Ok,
    // `#[cfg_attr(not(test), allow(dead_code))]`: 現状 `crate::tenant`・`crate::txn` の
    // 書き込み経路は commit 後の同期的な索引反映を持たず、production 側は常に `Ok` のみを
    // 構築する。本バリアントは応答一意性（反映失敗を成功応答へ転化させない）を
    // 検証するユニットテスト（本モジュール末尾）専用の到達点であり、通常ビルドでは
    // `dead_code` lint が発火するため黙殺する（`catalog.rs::insert_row_into_table` と
    // 同じ理由・同じパターン）。
    #[cfg_attr(not(test), allow(dead_code))]
    IndexReflectionFailed,
}

/// commit 成功直後から呼び出し元への応答確定までの区間を覆う Drop ガード
/// （RECOVER-5）。
///
/// - [`Self::armed`] で commit 直後にガードを有効化する。
/// - 応答を確定できる状態（成功で return できる状態）に達したら
///   [`Self::disarm`] を呼ぶ。以降 Drop しても abort しない。
/// - `disarm` を呼ばずに drop された場合（＝ armed のまま区間内で panic して
///   unwind してきた場合）、[`should_abort`] の判定に従い
///   `std::process::abort()` する。
///
/// abort 呼び出し本体と「abort すべきかの判定」（[`should_abort`]）を分離して
/// いるのは、abort 実行はプロセスを道連れにするため in-process では検証できず、
/// 判定側だけを純粋なユニットテストで検証可能にするため。
pub(crate) struct PostCommitPanicGuard {
    armed: bool,
}

impl PostCommitPanicGuard {
    /// commit 成功直後に呼び出し、ガードを有効化した状態で返す。
    pub(crate) fn armed() -> Self {
        Self { armed: true }
    }

    /// 応答確定（成功で return できる状態）に到達した合図。以降 Drop で
    /// abort しない。
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PostCommitPanicGuard {
    fn drop(&mut self) {
        if should_abort(self.armed, std::thread::panicking()) {
            // 固定の英語文言のみを stderr へ出す。テナント ID・テーブル名・行データ・
            // operation_id を一切含めない（security.md P0「エラー・ログ経由で他テナントの
            // データ・存在情報を漏らさない」）。
            crate::log_stderr!(
                "fatal: panic occurred after a write transaction committed successfully; \
                 aborting the process to avoid returning a false success response"
            );
            std::process::abort();
        }
    }
}

/// commit 成功後の保護区間内で panic が発生したか（＝ armed のまま unwind して
/// きたか）を純粋に判定する。`armed`（[`PostCommitPanicGuard`] が disarm 済みで
/// ないか）と `panicking`（現在 unwind 中か。呼び出し元は
/// `std::thread::panicking()` を渡す）の両方が真の場合のみ abort すべきと判定する
/// （fail-closed: 判定が曖昧な場合ではなく、armed かつ panicking が確定した場合の
/// みプロセスを止める設計であり、逆に armed が誤って解除された状態を作らないことが
/// 呼び出し元〔`commit_and_finish`〕の責務となる）。
pub(crate) fn should_abort(armed: bool, panicking: bool) -> bool {
    armed && panicking
}

/// このスレッドが現在「commit 成功後・応答未確定」の区間にあるかを読み取り専用で
/// 照会する（TASK-97、対象ビヘイビア: RECOVER-6）。[`panic_hook`](crate::recovery::
/// panic_hook) が panic フック内から呼び、緊急応答（wire-server 側が ERR-5 に
/// 従い `D`（detail）フィールドへ `state=may_be_committed` を載せた
/// ErrorResponse。ポインタ: TASK-153・ERR-5）を送出してよい状況かどうかの
/// 判定材料の一つに使う。
///
/// [`COMMIT_PENDING_RESPONSE`] をクリアしない・書き換えない（既存の
/// [`ResponseBoundaryGuard`] の世代管理・abort 判定ロジックには一切影響しない）。
/// 返り値は世代番号そのもの（`Some` なら pending 中）。他スレッド・他接続の値が
/// 混入しない（thread-local）のは前提どおりだが、**同一スレッド内で複数世代の
/// 混同が起こり得る**点に注意 ―― 本モジュールの `commit` は `crate::tenant`・
/// `crate::txn` 経由の書き込みだけでなく `crate::storage::Storage::put`/`put_batch`・
/// `crate::catalog` の DDL からも直接呼ばれる（モジュール冒頭コメント参照）。
/// `execute_sql_in_session` の呼び出し区間中にそれらの経路が commit した場合、
/// その commit の世代がここに現れうるが、それは緊急応答チャネルに登録された
/// バイト列が指す commit と同一とは限らない。呼び出し元（panic フック）は
/// この返り値を単独で「送信してよい」根拠にせず、
/// [`current_response_boundary_generation`] と突き合わせて一致した場合のみ
/// 送信する契約（[`crate::recovery::panic_hook::emergency_send_decision`]
/// 参照）。
pub(crate) fn active_commit_pending_generation() -> Option<u64> {
    COMMIT_PENDING_RESPONSE.with(|f| f.get())
}

/// このスレッドで現在アクティブな [`ResponseBoundaryGuard`] の世代番号を読み取り
/// 専用で照会する（TASK-97、対象ビヘイビア: RECOVER-6）。
/// [`crate::recovery::panic_hook::EmergencyResponseRegistration::register`] が
/// 登録時点でこの値を捕捉し、[`active_commit_pending_generation`] と突き合わせる
/// ための世代番号として保持する（緊急応答チャネルに登録されたバイト列が指す
/// commit と、実際に commit-pending になった世代が一致することを保証する。
/// `active_commit_pending_generation` のドキュメント参照）。
///
/// アクティブなガードが存在しない（0）場合は `None` を返す。`0` はガード未生成の
/// 予約値であり実世代として払い出されない（[`NEXT_RESPONSE_BOUNDARY_GENERATION`]
/// の初期値が `1` であることに対応）。
pub(crate) fn current_response_boundary_generation() -> Option<u64> {
    let generation = ACTIVE_RESPONSE_BOUNDARY_GENERATION.with(|c| c.get());
    if generation == 0 {
        None
    } else {
        Some(generation)
    }
}

/// commit 成功境界の公開 choke point（RECOVER-5）。`write_txn` は呼び出し元が
/// pre-commit の検証・書き込みを終え、commit 直前まで組み立て済みのトランザクション
/// を渡す契約（呼び出し元が `Err` を返す場合はこの関数を呼ばず `write_txn` を drop
/// する）。
///
/// 内部で [`crate::storage::prepare_generation_bump`]・
/// [`crate::storage::commit_prepared_write_txn`] の 2 段階で commit する
/// （世代カウントの経路網羅契約はそのまま維持する。実際の呼び出しは
/// [`commit_and_finish_with`] 経由）。この 2 段階に分けている理由・保護契約の
/// 詳細は [`commit_and_finish_with`] のドキュメント参照。
pub(crate) fn commit_and_finish<T>(
    write_txn: impl storage::CommitTxn,
    value: T,
    post_commit: impl FnOnce(&T) -> PostCommitResult,
) -> StorageResult<(T, PostCommitResult)> {
    // 書き込みゲートの permit（`storage::GatedWriteTxn`）は commit 完了まで保持する
    // （SQL-31・TASK-221。`_` で受けると即座に解放されるため名前付き束縛にする）。
    let (write_txn, _permit) = write_txn.split_for_commit();
    commit_and_finish_with(
        write_txn,
        value,
        post_commit,
        storage::prepare_generation_bump,
        storage::commit_prepared_write_txn,
    )
}

/// [`commit_and_finish`] の実装本体。`prepare_fn`・`commit_fn` を差し替え可能に
/// しているのはテスト専用（production は常に [`crate::storage::
/// prepare_generation_bump`]・[`crate::storage::commit_prepared_write_txn`] を
/// 渡す）。この 2 引数への分割自体が、`write_txn.commit()` という 1 回の呼び出しを
/// 境に durable 判定の可否が変わるという事実に対応している。
///
/// - `prepare_fn` は `write_txn.commit()` を呼ぶ**前**の準備段階を表す。まだ
///   commit していないため、`Err` を返しても durable write は確定的に発生して
///   いない ―― 通常のエラー応答として呼び出し元へ返してよい（ガードの arm も
///   pending の記録も行わない）。
/// - `commit_fn` は `write_txn.commit()` そのものを表す。ここから先は
///   [`PostCommitPanicGuard`] を arm し、そのスレッドにアクティブな
///   [`ResponseBoundaryGuard`] が存在する場合はその世代番号を
///   [`COMMIT_PENDING_RESPONSE`] へ記録してから呼び出す（codex-review P1
///   再指摘・PR #246 対応。`write_txn.commit()` を呼んでから戻るまでの間に
///   panic した場合、redb が内部で durable write を終えているか否かは呼び出し
///   元からは判別できない。fail-closed の方針に従い、この曖昧な区間も commit
///   成功後と同じ扱いで保護区間に含める）。
///   - `commit_fn` が `Err` を返した場合も、redb バックエンドの commit 呼び出し
///     自体が返す `Err` は「durable write が発生しなかった」ことを意味しない
///     （I/O・同期エラーでは新旧どちらの状態が永続化されたか呼び出し元からは
///     判別できない。codex-review P1 再指摘・PR #246 対応）。そのため通常の
///     エラー応答は返さず、`std::process::abort()` して結果不確定のまま
///     成功・失敗いずれの応答も送出させない（panic 時の
///     [`PostCommitPanicGuard`]・[`ResponseBoundaryGuard`] と同じ fail-closed
///     方針。`abort()` はプロセスを直ちに終了させ Drop を走らせないため、
///     この分岐でガード・pending フラグの後始末は行わない＝行っても無意味）。
///   - `commit_fn` が `Ok` を返した場合はガードを armed のまま `post_commit` を
///     実行し、正常に完了できたら disarm してから `(value, post_commit の結果)`
///     を返す（`post_commit` 内で panic した場合は `PostCommitPanicGuard` の
///     Drop が abort する。`post_commit` 完了後・呼び出し元が応答を確定するまで
///     の区間は `COMMIT_PENDING_RESPONSE` を経由して [`ResponseBoundaryGuard`]
///     が引き続き保護する）。
fn commit_and_finish_with<T>(
    write_txn: redb::WriteTransaction,
    value: T,
    post_commit: impl FnOnce(&T) -> PostCommitResult,
    prepare_fn: impl FnOnce(&redb::WriteTransaction) -> StorageResult<()>,
    commit_fn: impl FnOnce(redb::WriteTransaction) -> StorageResult<()>,
) -> StorageResult<(T, PostCommitResult)> {
    prepare_fn(&write_txn)?;

    let active_guard_generation = ACTIVE_RESPONSE_BOUNDARY_GENERATION.with(|c| c.get());
    if active_guard_generation != 0 {
        COMMIT_PENDING_RESPONSE.with(|f| f.set(Some(active_guard_generation)));
    }
    let guard = PostCommitPanicGuard::armed();

    match commit_fn(write_txn) {
        Ok(()) => {
            let post_commit_result = post_commit(&value);
            guard.disarm();
            Ok((value, post_commit_result))
        }
        Err(_backend_commit_error) => {
            // 固定の英語文言のみを stderr へ出す。テナント ID・テーブル名・行データ・
            // operation_id を一切含めない（security.md P0）。バックエンドのエラー詳細も
            // 出さない ―― durable かどうか不明な状態でエラー内容から情報が漏れることを
            // 避ける。
            crate::log_stderr!(
                "fatal: the storage backend's commit call returned an error, but whether the \
                 write became durable before the error is indeterminate; aborting the process \
                 to avoid returning a possibly-incorrect success or failure response"
            );
            std::process::abort();
        }
    }
}

/// [`commit_and_finish`] の薄いラッパ。`crate::tenant` の各 `*_unchecked`
/// 関数（commit 後の派生状態反映を持たない）から呼ばれる想定で、
/// `post_commit` は常に [`PostCommitResult::Ok`] を返す no-op とする。
pub(crate) fn commit(write_txn: impl storage::CommitTxn) -> StorageResult<()> {
    commit_and_finish(write_txn, (), |()| PostCommitResult::Ok).map(|((), _)| ())
}

/// `crate::txn::WriteTxn`/`BatchWriteTxn` の commit 一本化点
/// （`crate::storage::commit_write_txn` と同じ `has_writes` 契約）から呼ばれる。
/// `has_writes == false` の場合は commit させず abort する既存契約
/// （[`crate::storage::commit_write_txn`] 参照）をそのまま踏襲する ――
/// commit していない区間には保護対象の point of no return が存在しないため、
/// ガードを arm する必要がない。`has_writes == true` の場合のみ
/// [`commit`] 経由でガード区間に載せる。
pub(crate) fn commit_write_txn_guarded(
    write_txn: impl storage::CommitTxn,
    has_writes: bool,
) -> StorageResult<()> {
    if has_writes {
        commit(write_txn)
    } else {
        storage::commit_write_txn(write_txn, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    // --- should_abort の全分岐（RECOVER-5 の判定純関数）---

    #[test]
    fn should_abort_true_only_when_armed_and_panicking() {
        assert!(should_abort(true, true));
        assert!(!should_abort(true, false));
        assert!(!should_abort(false, true));
        assert!(!should_abort(false, false));
    }

    // --- ガードの正常系: disarm 済みなら drop しても abort しない ---
    // （abort すればテストプロセスごと落ちるため、「abort しなかった」ことは
    // このテスト自身が最後まで完走することで示される）

    #[test]
    fn guard_disarmed_does_not_abort_on_drop() {
        let guard = PostCommitPanicGuard::armed();
        guard.disarm();
        // drop はスコープ終端で暗黙的に走る。abort されないことをテスト完走で示す。
    }

    // --- commit_and_finish: RECOVER-5 応答一意性 ---
    // 派生状態反映が失敗しても、commit 済みの成功応答は Err へ転化しない。

    #[test]
    fn commit_and_finish_keeps_success_even_if_post_commit_reflection_fails() {
        let path = unique_db_path("commit-boundary-post-commit-fail");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let write_txn = storage.db().begin_write().expect("begin_write");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0],
            metadata: &[],
        };
        {
            let mut table = write_txn
                .open_table(crate::storage::ROWS_TABLE)
                .expect("open rows table");
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 1u64), encoded.as_slice())
                .expect("insert");
        }

        let (value, post_commit_result) =
            commit_and_finish(write_txn, 1u64, |_| PostCommitResult::IndexReflectionFailed)
                .expect("commit must succeed even though post_commit reports a failure");

        assert_eq!(value, 1u64);
        assert_eq!(post_commit_result, PostCommitResult::IndexReflectionFailed);

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        drop(storage);
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 1u64)
            .expect("committed row must remain visible after reopen");
    }

    // --- RECOVER-5 再指摘（PR #246 #discussion_r3870845012）の回帰テスト ---
    // `ResponseBoundaryGuard` の外側（＝ アクティブなガードなし）で commit した
    // stale なフラグを、後続クエリのガードが誤って自分の commit と関連付けて
    // abort しないこと。世代番号の紐付けにより、このケースは in-process で
    // 「abort しない」ことを検証できる（`catch_unwind` 内でガードが drop され、
    // `std::thread::panicking()` は true になるが、pending 世代がこのガードの
    // 世代と一致しないため abort 判定は false のまま）。旧実装（無条件フラグ）
    // では stale フラグが誤ってこのガードの commit と誤認され、テストプロセス
    // ごと SIGABRT していた。

    #[test]
    fn commit_outside_any_guard_does_not_arm_a_later_unrelated_guard_panic() {
        let path = unique_db_path("commit-boundary-stale-flag-no-guard");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // アクティブな ResponseBoundaryGuard が存在しないスレッド上で commit する
        // （wire 層を経由しない公開 API 呼び出しの模擬。例: `Storage::put` を
        // ガード外から直接呼ぶ経路）。
        let write_txn = storage.db().begin_write().expect("begin_write");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: &[],
        };
        {
            let mut table = write_txn
                .open_table(crate::storage::ROWS_TABLE)
                .expect("open rows table");
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 42u64), encoded.as_slice())
                .expect("insert");
        }
        commit(write_txn).expect("commit outside any guard must succeed");

        // 後続の（無関係な）クエリを模した区間: ガードを生成し、commit を伴わずに
        // panic する。stale フラグが誤って紐付いていなければ abort しないはず。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _response_boundary = ResponseBoundaryGuard::new();
            panic!("later unrelated request panics before any commit of its own");
        }));

        assert!(
            result.is_err(),
            "the injected panic must still propagate as a normal unwind (not abort)"
        );

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        drop(storage);
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 42u64)
            .expect("committed row must remain visible after reopen");
    }

    // --- codex-review P1 再指摘（PR #246 #discussion_r3873683862）の回帰テスト ---
    // 外側ガード配下で commit した後にネストして内側ガードを生成・正常 drop しても、
    // 外側の pending フラグ（COMMIT_PENDING_RESPONSE）が消去されないこと
    // （非所有設計により内側の drop はスレッドローカル状態に一切触れない）。
    // 旧実装（drop 時に世代を問わず無条件で `replace(None)`）では、この内側の
    // 正常 drop だけで外側の保護区間が消去されてしまっていた。

    #[test]
    fn nested_guard_normal_drop_does_not_clear_outer_pending_flag() {
        let path = unique_db_path("commit-boundary-nested-guard-state");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let outer = ResponseBoundaryGuard::new();

        let write_txn = storage.db().begin_write().expect("begin_write");
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: &[],
        };
        {
            let mut table = write_txn
                .open_table(crate::storage::ROWS_TABLE)
                .expect("open rows table");
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 100u64), encoded.as_slice())
                .expect("insert");
        }
        commit(write_txn).expect("outer commit must succeed");

        let outer_generation = outer
            .owned_generation
            .expect("outer guard must own the boundary");
        assert_eq!(
            COMMIT_PENDING_RESPONSE.with(|f| f.get()),
            Some(outer_generation),
            "outer commit must record its own generation as pending"
        );

        // 内側ガードを生成・commit を伴わずに正常 drop する（ネストケース）。
        {
            let inner = ResponseBoundaryGuard::new();
            assert_eq!(
                inner.owned_generation, None,
                "nested inner guard must not own the boundary"
            );
            // drop はスコープ終端で暗黙的に走る。
        }

        // 内側の正常 drop 後も、外側の pending フラグは消去されていないこと。
        assert_eq!(
            COMMIT_PENDING_RESPONSE.with(|f| f.get()),
            Some(outer_generation),
            "nested inner guard's normal drop must not clear the outer's pending flag"
        );

        // 外側ガードを drop してテスト自身が pending 状態を残さないようにする
        // （テストプロセスが `--test-threads=1` 等で共有スレッドを使う場合、
        // 残留した pending 世代が後続の無関係なガードの誤 abort を招きうるため）。
        drop(outer);
        assert_eq!(
            COMMIT_PENDING_RESPONSE.with(|f| f.get()),
            None,
            "outer guard's own drop must clear the pending flag it owns"
        );

        drop(storage);
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 100u64)
            .expect("committed row must remain visible after reopen");
    }

    // --- commit_write_txn_guarded: has_writes == false は commit させず abort する ---

    #[test]
    fn commit_write_txn_guarded_no_writes_does_not_bump_generation() {
        let path = unique_db_path("commit-boundary-no-writes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let before = storage.current_generation().expect("read generation");
        let write_txn = storage.db().begin_write().expect("begin_write");
        commit_write_txn_guarded(write_txn, false).expect("no-op commit must succeed");
        let after = storage.current_generation().expect("read generation");

        assert_eq!(
            before, after,
            "no-writes commit must not advance generation"
        );
    }

    // --- RECOVER-5: commit 成功後 panic のプロセス終了保証（サブプロセス検証）---
    //
    // `PostCommitPanicGuard` の abort 実行はプロセスを道連れにするため in-process では
    // 検証できない。`std::env::current_exe()`（cargo test が生成する本クレートの
    // テストバイナリ自身）を、環境変数付きでこのテスト関数だけをフィルタして
    // 再実行する自己再帰構造で子プロセス化する（`examples/crash_tool.rs` の
    // プロセス外検証パターンと同型。新規バイナリ・feature ゲート API は追加しない
    // ―― 子プロセスは本クレートの既存テストバイナリそのもの）。
    const CHILD_DB_ENV: &str = "ENGINE_COMMIT_BOUNDARY_PANIC_CHILD_DB";

    #[test]
    fn subprocess_post_commit_panic_aborts_before_returning_success() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV) {
            // 子プロセス側: 実際に行を commit したうえで post_commit 内で panic を
            // 注入し、`PostCommitPanicGuard` の abort へ委ねる。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            };
            {
                let mut table = write_txn
                    .open_table(crate::storage::ROWS_TABLE)
                    .expect("child: open rows table");
                let encoded = crate::storage::encode_row(&row).expect("child: encode row");
                table
                    .insert(("tenant-a", 7u64), encoded.as_slice())
                    .expect("child: insert");
            }
            let _ = commit_and_finish(write_txn, (), |()| {
                panic!("injected post-commit panic for RECOVER-5 verification")
            });
            // ここへ到達したのはガードが abort しなかった不具合。親側が判定できる
            // 固定マーカーを出してから異常終了する。
            println!("CHILD_REACHED_AFTER_PANIC_GUARD");
            std::process::exit(1);
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-panic");
        let _cleanup = CleanupGuard(path.clone());
        // 子が開けるよう、DB ファイルを先に作ってから閉じる。
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_post_commit_panic_aborts_before_returning_success",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV, &path)
            .stdout(std::process::Stdio::piped())
            // stderr は破棄する（`Stdio::null()`）。テストは stdout の固定マーカーと
            // 終了コード・シグナルのみを検証し stderr の内容を読まないため、pipe のまま
            // 放置すると panic hook の出力（backtrace 等。`RUST_BACKTRACE` 有効時に
            // 顕著）で OS のパイプバッファが埋まり、子が `abort()` 前に write でブロック
            // して親が下記 30 秒タイムアウトで誤って失敗しうる（flaky の温床。
            // cursor[bot] レビュー指摘対応・PR #246）。親側は try_wait のみをポーリング
            // し pipe を能動的に読まない設計のため、そもそも pipe せず捨てるのが最小修正。
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

        // CI ハング防止のタイムアウト（`wait_timeout` 等の追加依存を使わず、
        // `try_wait` をポーリングして上限超過時は kill する）。
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

        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_PANIC_GUARD"),
            "guard failed to abort before returning a response: stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            // `std::process::abort()` は Unix では SIGABRT（== 6）で終了する契約。
            // libc への新規依存を避け、数値を直接比較する。
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort); \
                 status={status:?} stdout={stdout_buf}"
            );
        }

        // commit 自体は成功しているため、再オープン後も行が可視であること
        // （commit 後の panic であり、副作用〔commit〕自体は残る ――
        // 保証対象は「commit しないこと」ではなく「成功応答を返さないこと」）。
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 7u64)
            .expect("committed row must remain visible after subprocess abort");
    }

    // --- codex-review P1 再指摘（PR #246）の回帰テスト ---
    // prepare_fn（write_txn.commit() より前の準備段階）の Err は、まだ commit を
    // 呼んでいないため durable write が確定的に発生していない。ガードの arm も
    // pending の記録も一切行わずに通常の Err として伝播すること。

    #[test]
    fn prepare_fn_err_propagates_normally_without_arming_the_boundary() {
        let path = unique_db_path("commit-boundary-prepare-fn-err-no-arm");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let outer = ResponseBoundaryGuard::new();

        let write_txn = storage.db().begin_write().expect("begin_write");
        let result = commit_and_finish_with(
            write_txn,
            (),
            |()| PostCommitResult::Ok,
            |_txn| Err(crate::storage::StorageError::GenerationCounterOverflow),
            |_txn| panic!("commit_fn must not be called when prepare_fn already returned Err"),
        );

        assert!(
            result.is_err(),
            "prepare_fn returning Err must propagate as Err"
        );
        assert_eq!(
            COMMIT_PENDING_RESPONSE.with(|f| f.get()),
            None,
            "a prepare_fn Err must never arm the pending flag (commit_fn was never reached)"
        );

        // pending が一切立っていないため、この後 outer の区間内で panic しても
        // abort しないはず（commit_fn へ到達していないため）。
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("panic after a prepare_fn Err must not be treated as post-commit");
        }));
        assert!(
            panicked.is_err(),
            "the injected panic must still unwind normally"
        );

        drop(outer);
    }

    // --- codex-review P1 再指摘（PR #246）の end-to-end 回帰テスト ---
    // `write_txn.commit()` 呼び出しから戻るまでの間（redb が durable write
    // を終えたかどうか呼び出し元から判別できない曖昧な区間）に panic した場合も、
    // 「commit 成功後の panic」と同じ扱いで abort すること。実際の redb 呼び出し内部
    // に panic を注入することはできないため、`commit_and_finish_with` の `commit_fn`
    // 引数を panic するクロージャに差し替えて同じ曖昧区間を再現する
    // （`commit_and_finish` 自体は production と同じ `PostCommitPanicGuard` の
    // arm・`COMMIT_PENDING_RESPONSE` の記録タイミングをそのまま経由する）。
    const CHILD_DB_ENV_MID_COMMIT: &str = "ENGINE_COMMIT_BOUNDARY_MID_COMMIT_PANIC_CHILD_DB";

    #[test]
    fn subprocess_panic_during_commit_call_itself_still_aborts() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV_MID_COMMIT) {
            // 子プロセス側: 外側ガードを生成してから `commit_and_finish_with` を
            // `commit_fn` が panic するクロージャで呼ぶ。production の
            // `commit_and_finish` は `commit_fn` を呼ぶ前にガード・pending を
            // 準備するため、この panic は「commit 呼び出し内部の曖昧な panic」を
            // 直接再現する（`write_txn` 自体は実 DB から取得するが、`commit_fn` は
            // これを使わずに panic するため commit は実行されない）。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");

            let _response_boundary = ResponseBoundaryGuard::new();
            let _ = commit_and_finish_with(
                write_txn,
                (),
                |()| PostCommitResult::Ok,
                |_txn| Ok(()),
                |_txn| panic!("injected panic inside the commit call itself"),
            );
            println!("CHILD_REACHED_AFTER_MID_COMMIT_PANIC_GUARD");
            std::process::exit(1);
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-mid-commit-panic");
        let _cleanup = CleanupGuard(path.clone());
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_panic_during_commit_call_itself_still_aborts",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV_MID_COMMIT, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

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

        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_MID_COMMIT_PANIC_GUARD"),
            "guard failed to abort for a panic inside the commit call itself: stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort) for a panic \
                 inside the commit call itself; status={status:?} stdout={stdout_buf}"
            );
        }
    }

    // --- codex-review P1 再指摘（PR #246・結果不明な backend commit エラー）の
    // end-to-end 回帰テスト ---
    // `commit_fn`（= `write_txn.commit()` 呼び出しそのもの）が `Err` を返した場合、
    // その `Err` は「durable write が発生しなかった」ことを保証しない（I/O・同期
    // エラーでは新旧どちらの状態が永続化されたか判別できない）。そのため通常の
    // エラー応答は返さず `std::process::abort()` すること（panic ではなく通常の
    // `Err` 戻り値であっても、この分岐だけは fail-closed に abort する）。
    // ここでの合成 `Err`（`prepare_fn` は成功させ `commit_fn` だけが `Err` を返す）
    // は「commit 呼び出しが実際に叩かれてから失敗した」ことを模擬しており、
    // `prepare_fn_err_propagates_normally_without_arming_the_boundary`（commit を
    // 一度も呼ばずに失敗する経路）とは別の分岐を検証する。
    const CHILD_DB_ENV_COMMIT_FN_ERR: &str = "ENGINE_COMMIT_BOUNDARY_COMMIT_FN_ERR_CHILD_DB";

    #[test]
    fn subprocess_commit_fn_returning_err_aborts_instead_of_a_normal_error_response() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV_COMMIT_FN_ERR) {
            // 子プロセス側: `prepare_fn` は成功させ、`commit_fn`（= 実際の commit
            // 呼び出しに相当する箇所）だけが合成エラーを返す。production の
            // `commit_and_finish` はこの `Err` を通常の `Err` として伝播せず、
            // `commit_and_finish_with` 内で直接 `std::process::abort()` する。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");

            let _response_boundary = ResponseBoundaryGuard::new();
            let _ = commit_and_finish_with(
                write_txn,
                (),
                |()| PostCommitResult::Ok,
                |_txn| Ok(()),
                |_txn| Err(crate::storage::StorageError::GenerationCounterOverflow),
            );
            // ここへ到達したのは fail-closed 化されていない不具合。親側が判定できる
            // 固定マーカーを出してから異常終了する。
            println!("CHILD_REACHED_AFTER_COMMIT_FN_ERR_WITHOUT_ABORT");
            std::process::exit(1);
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-commit-fn-err");
        let _cleanup = CleanupGuard(path.clone());
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_commit_fn_returning_err_aborts_instead_of_a_normal_error_response",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV_COMMIT_FN_ERR, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

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

        assert!(
            !stdout_buf.contains("CHILD_REACHED_AFTER_COMMIT_FN_ERR_WITHOUT_ABORT"),
            "a commit_fn Err must abort instead of returning a normal error response: \
             stdout={stdout_buf}"
        );
        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort) for a commit_fn \
                 Err (indeterminate durability); status={status:?} stdout={stdout_buf}"
            );
        }
    }

    // --- RECOVER-5 拡張区間: commit_and_finish が正常 return（disarm 済み）した
    // 後、`ResponseBoundaryGuard` がまだ生存している区間（＝ wire 層が応答を組み立て・
    // 送信する区間の模擬）で panic しても abort すること。
    // codex-review P1・PR #246 指摘（`commit_and_finish` の disarm 直後から応答確定
    // までが未保護だった点）の回帰テスト。
    const CHILD_DB_ENV_RESPONSE_BOUNDARY: &str =
        "ENGINE_COMMIT_BOUNDARY_RESPONSE_BOUNDARY_PANIC_CHILD_DB";

    #[test]
    fn subprocess_panic_after_commit_and_finish_returns_still_aborts_within_response_boundary() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV_RESPONSE_BOUNDARY) {
            // 子プロセス側: wire 層（`simple_query::execute_and_respond`）が
            // クエリ処理の入口で `ResponseBoundaryGuard` を生成してから
            // `commit_and_finish` を呼び、正常 return を受け取った後（＝ 応答の
            // 組み立て・送信中を模擬する区間）で panic する、という呼び出し順を
            // 直接再現する。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let write_txn = storage.db().begin_write().expect("child: begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            };
            {
                let mut table = write_txn
                    .open_table(crate::storage::ROWS_TABLE)
                    .expect("child: open rows table");
                let encoded = crate::storage::encode_row(&row).expect("child: encode row");
                table
                    .insert(("tenant-a", 9u64), encoded.as_slice())
                    .expect("child: insert");
            }

            let _response_boundary = ResponseBoundaryGuard::new();
            let (_, post_commit_result) =
                commit_and_finish(write_txn, (), |()| PostCommitResult::Ok)
                    .expect("child: commit_and_finish must succeed");
            assert_eq!(post_commit_result, PostCommitResult::Ok);

            // `commit_and_finish` は既に disarm 済みで正常 return している
            // （旧実装ならここから先は無保護）。`_response_boundary` はまだ
            // 生存中のため、ここで panic しても abort するはずである。
            panic!("injected panic in the wire-layer response-assembly window (post commit_and_finish)");
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-response-boundary-panic");
        let _cleanup = CleanupGuard(path.clone());
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_panic_after_commit_and_finish_returns_still_aborts_within_response_boundary",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV_RESPONSE_BOUNDARY, &path)
            .stdout(std::process::Stdio::piped())
            // 上のテスト同様、stderr は pipe せず破棄する（パイプバッファ詰まり回避）。
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

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

        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort) even though the \
                 panic occurred after commit_and_finish already returned; \
                 status={status:?} stdout={stdout_buf}"
            );
        }

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 9u64)
            .expect("committed row must remain visible after subprocess abort");
    }

    // --- codex-review P1 再指摘（PR #246 #discussion_r3873683862）の end-to-end
    // 回帰テスト ---
    // 外側ガード配下で commit した後、ネストして内側ガードを生成・commit を伴わず
    // 正常 drop してから、外側ガードの保護区間内で panic した場合に abort する
    // こと（内側ガードの生成・正常 drop が外側の保護を消し去らないことの
    // end-to-end 検証。上の `nested_guard_normal_drop_does_not_clear_outer_pending_flag`
    // が in-process の状態検証、本テストが abort 契約そのものの検証を担う）。
    const CHILD_DB_ENV_NESTED_GUARD: &str = "ENGINE_COMMIT_BOUNDARY_NESTED_GUARD_CHILD_DB";

    #[test]
    fn subprocess_outer_guard_still_aborts_after_nested_inner_guard_normal_drop() {
        if let Ok(db_path) = std::env::var(CHILD_DB_ENV_NESTED_GUARD) {
            // 子プロセス側: 外側ガード配下で commit した後、ネストして内側ガードを
            // 生成し（commit を伴わず）正常 drop する。その後、外側ガードがまだ
            // 生存している区間で panic する。
            let storage = Storage::open(&db_path).expect("child: open storage");
            let outer = ResponseBoundaryGuard::new();

            let write_txn = storage.db().begin_write().expect("child: begin_write");
            let row = RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            };
            {
                let mut table = write_txn
                    .open_table(crate::storage::ROWS_TABLE)
                    .expect("child: open rows table");
                let encoded = crate::storage::encode_row(&row).expect("child: encode row");
                table
                    .insert(("tenant-a", 11u64), encoded.as_slice())
                    .expect("child: insert");
            }
            commit(write_txn).expect("child: outer commit must succeed");

            {
                // ネストして生成された内側ガード。commit を伴わず正常 drop する
                // （旧実装ではこの drop だけで外側の pending フラグが消えていた）。
                let _inner = ResponseBoundaryGuard::new();
            }

            // `outer` を明示的に drop せず、ここで panic して unwind に乗せる。
            // 内側ガードの生成・正常 drop を経ても外側 `outer` の pending
            // フラグが残っていれば、この unwind による `outer` の drop で
            // abort するはずである。
            let _ = &outer;
            panic!(
                "injected panic within outer guard's boundary, after a nested inner \
                 guard was created and dropped normally"
            );
        }

        // 親プロセス側。
        let path = unique_db_path("commit-boundary-subprocess-nested-guard");
        let _cleanup = CleanupGuard(path.clone());
        drop(Storage::open(&path).expect("parent: create storage"));

        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(&exe)
            .arg("--exact")
            .arg(
                "recovery::commit_boundary::tests::\
                 subprocess_outer_guard_still_aborts_after_nested_inner_guard_normal_drop",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_DB_ENV_NESTED_GUARD, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child process");

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

        assert!(
            !status.success(),
            "child process must not exit successfully; status={status:?} stdout={stdout_buf}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(
                status.signal(),
                Some(6),
                "child must be terminated by SIGABRT (std::process::abort) even though a \
                 nested inner guard was created and dropped normally in between; \
                 status={status:?} stdout={stdout_buf}"
            );
        }

        // commit 自体は成功しているため、再オープン後も行が可視であること。
        let reopened = Storage::open(&path).expect("reopen storage");
        reopened
            .get("tenant-a", 11u64)
            .expect("committed row must remain visible after subprocess abort");
    }
}
