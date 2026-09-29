//! 明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31・TASK-221）の構文
//! 受理と、セッション単位の状態機械 [`SessionTransaction`] を提供する。
//!
//! `wire-server` の簡易クエリ（`simple_query.rs`）・拡張クエリ
//! （`extended_query.rs`）の両経路が、接続（セッション）ごとに 1 つの
//! [`SessionTransaction`] を保持し、`core::EngineCore::execute_sql_in_txn`／
//! `execute_parsed_in_txn` へ `&mut` で渡す。`Idle`（トランザクション外）の間は
//! 既存の autocommit 経路（[`crate::core::EngineCore::execute_sql_in_session`]）と
//! ビット同一に振る舞い、`Active` の間だけ共有 `redb::WriteTransaction`
//! （[`crate::storage::Storage::begin_explicit_write_txn`]）を保持して複数の
//! 書き込み文を 1 回の commit にまとめる。
//!
//! commit 成功境界（`RECOVER-5`・`RECOVER-6`）: point of no return は `COMMIT`
//! 文が呼ぶ [`crate::recovery::commit_boundary::commit`] の 1 回のみ。台帳
//! （`RECOVER-12`）は行書き込みと同一の共有 `write_txn` 内に記録されるため、
//! `ROLLBACK`・接続断（`SessionTransaction` の drop）で行・台帳とも一括して
//! 消える。
//!
//! 暗黙トランザクション（Issue #1175・WIRE-16）: `BEGIN` を含まない複数文メッセージの
//! うち書き込みが最後以外にある形は、wire 層が [`SessionTransaction::begin_implicit`] で
//! メッセージ全体を 1 つのトランザクションにして原子的に実行する。起源（`TxnOrigin`）が
//! `Implicit` の `Active` は明示トランザクションと同じ許可リストで文を実行するが、
//! クライアントが `ROLLBACK` を発行できないため、エラー・上限超過では `Failed` ではなく
//! `Idle` へ戻り、メッセージをまたがない。
//!
//! 読み取りの既知の逸脱（SQL-31 の完全な意味論からの意図的な縮退）: 本実装は
//! 「同一トランザクション内で自分がまだ書き込んでいないテーブル」の読み取りのみ
//! 通常経路で許可し、既に書き込んだテーブルへの読み取りは `0A000` で拒否する
//! （自トランザクションの未 commit 変更の可視化は対象外）。`written_tables` が
//! この判定に使う集合。詳細は `docs/design/explicit-transaction.md` 参照。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;

/// `BEGIN`/`COMMIT`/`ROLLBACK` の種別（構造検証のみ。実行は
/// [`SessionTransaction`] が担う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnControl {
    Begin,
    Commit,
    Rollback,
}

/// `BEGIN`/`COMMIT`/`ROLLBACK` の規範形（`[WORK|TRANSACTION]` 修飾語のみ許容。
/// 分離レベル指定・`READ ONLY`・`AND CHAIN`・`$n` 等は `42601`）を受理する。
/// 先頭トークンが `BEGIN`/`COMMIT`/`ROLLBACK`（大小無視）であることは
/// 呼び出し元（`core.rs::parse_tokens`）が確認済みの前提。
pub(crate) fn validate_transaction_control_tokens(
    tokens: &[Token],
) -> Result<TxnControl, SqlSurfaceError> {
    let Some(Token::Ident(head)) = tokens.first() else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };
    let ctrl = if head.eq_ignore_ascii_case("BEGIN") {
        TxnControl::Begin
    } else if head.eq_ignore_ascii_case("COMMIT") {
        TxnControl::Commit
    } else if head.eq_ignore_ascii_case("ROLLBACK") {
        TxnControl::Rollback
    } else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };

    // 受信 SQL 由来のトークン列のため添字スライスを使わず `split_first` で進める。
    let rest = tokens.split_first().map_or(&[][..], |(_, tail)| tail);
    let rest = match rest.split_first() {
        Some((Token::Ident(kw), tail))
            if kw.eq_ignore_ascii_case("WORK") || kw.eq_ignore_ascii_case("TRANSACTION") =>
        {
            tail
        }
        _ => rest,
    };
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after transaction control statement",
        ));
    }
    Ok(ctrl)
}

/// SQL テキストの先頭トークンが `ROLLBACK`（大小無視）かどうか。明示トランザクションが
/// `Failed` の間に受理してよい文かを parse 前に判定するために使う（SQL-31・TASK-221。
/// `core::EngineCore::execute_sql_in_txn`、wire-server の拡張クエリ Parse が呼ぶ）。
/// `ROLLBACK` の規範形の検証は通常どおり parse に任せる。字句解析に失敗した入力は
/// `false`（fail-closed。`25P02` で拒否される側に倒す）。
pub fn is_rollback_statement(sql: &str) -> bool {
    matches!(
        crate::sql::lexer::tokenize(sql).ok().as_deref().and_then(|t| t.first()),
        Some(Token::Ident(name)) if name.eq_ignore_ascii_case("ROLLBACK")
    )
}

/// [`SessionTransaction`] の対外的な状態（WIRE-19。`wire-server::result_encoder::
/// encode_ready_for_query` が `ReadyForQuery` の状態バイト `'I'`／`'T'`／`'E'`へ
/// 写像する入口。PR #1041 レビュー指摘対応で `#943` の担当分を本 PR へ吸収し、
/// 簡易・拡張クエリ両プロトコルへ結線済み）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    InTransaction,
    Failed,
}

/// `Active` なトランザクションの起源（Issue #1175）。`Explicit` はクライアントの
/// `BEGIN` で始まり、エラーで `Failed` へ遷移して `ROLLBACK` を待つ。`Implicit` は
/// `BEGIN` を含まない複数文メッセージを原子的に実行するために wire 層が
/// [`SessionTransaction::begin_implicit`] で開始するもので、メッセージの中で必ず
/// 終了する（成功なら [`SessionTransaction::commit_implicit`]、エラーなら
/// [`SessionTransaction::fail`] 等が `Idle` へ戻す）。クライアントが発行していない
/// `ROLLBACK` を待つ `Failed` に接続が固まることは無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxnOrigin {
    Explicit,
    Implicit,
}

/// トランザクション中に開いている共有書き込みトランザクションと、その
/// 付随状態。`write_txn`（[`crate::storage::GatedWriteTxn`]）は書き込みゲートの
/// permit を内包し、drop 時は abort してから permit を解放する。
struct ActiveTxn<'e> {
    origin: TxnOrigin,
    write_txn: crate::storage::GatedWriteTxn,
    started_at: Instant,
    statements: u32,
    seen_operation_ids: HashSet<String>,
    written_tables: HashSet<String>,
    /// このトランザクション内で書き込んだ `(tenant_id, table)` の組の集合
    /// （TABLE-17・TASK-205、Issue #1077）。COMMIT 時の遅延 `FOREIGN KEY` 検査
    /// （[`crate::constraint::enforce_deferred_foreign_keys_in_txn`]）が対象範囲を
    /// 導出する唯一の情報源——`written_tables`（テーブル名のみ）と違い、書き込みは
    /// 常にサーバー側導出テナントに紐づくため、ここでもテナントを明示的に持つ。
    /// [`Self::written_tables`] と異なる集合を分けて持つ理由: 前者は「読み取りを
    /// 拒否するテーブル名」の判定にしか使わず、テナントを持たないため COMMIT 時
    /// 検査の走査範囲（テナント別の物理キー範囲）を復元できない。記録漏れは
    /// 検査漏れ＝fail-open のバグになるため、書き込みのたびに
    /// [`Self::mark_written`] を必ず呼ぶ契約とする（呼び出し元は `core.rs` の
    /// INSERT／TRUNCATE の 2 箇所のみ）。
    written_by_tenant: std::collections::BTreeSet<(String, String)>,
    has_writes: bool,
    session_at_begin: SessionState,
    /// `DECLARE`/`FETCH`/`CLOSE`（WIRE-15・TASK-218）が開いたカーソルの集合。
    /// `ActiveTxn` に埋め込まれているため、トランザクションの終了（`commit`／
    /// `rollback`／`fail`。いずれも `ActiveTxn` を `mem::replace` で取り出して
    /// drop する）とともに保持していたカーソルもすべて自動的に消える
    /// （カーソルの寿命がトランザクションの寿命に一致する設計。
    /// `sql::cursor` モジュールドキュメント参照）。
    cursors: crate::sql::cursor::CursorRegistry,
    _marker: std::marker::PhantomData<&'e ()>,
}

enum TxnState<'e> {
    Idle,
    Active(Box<ActiveTxn<'e>>),
    /// `expired` は、持続時間の上限超過（[`SessionTransaction::release_if_expired`]）
    /// で `Failed` へ遷移し、まだ `54000` をクライアントへ報告していないことを表す。
    Failed {
        session_at_begin: SessionState,
        expired: bool,
    },
}

/// 持続時間・文数の上限超過時のエラー文言（`54000`）。
const LIMIT_EXCEEDED_MESSAGE: &str = "transaction exceeded duration or statement limit";

/// トランザクションの持続時間・文数の上限（実装既定値。spec 由来の数値ではない。
/// `docs/design/explicit-transaction.md` 参照）。
#[derive(Debug, Clone, Copy)]
pub struct TransactionLimits {
    pub max_duration: Duration,
    pub max_statements: u32,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(20),
            max_statements: 1_000,
        }
    }
}

/// 接続（セッション）ごとに 1 つ保持する明示トランザクションの状態機械。
pub struct SessionTransaction<'e> {
    state: TxnState<'e>,
    limits: TransactionLimits,
    /// `BEGIN`（`Idle` → `Active`）が成功するたびに 1 つ進む世代カウンタ
    /// （`TxnState` を跨いでセッション全体で単調増加。`COMMIT`／`ROLLBACK`で
    /// `ActiveTxn` が捨てられても値自体は保持し続ける）。wire-server 拡張
    /// クエリの portal が、カーソル `FETCH` の中断保持分を後から再送出して
    /// よいか（同一トランザクション内のままか）を突き合わせるために使う
    /// （PR #1049 レビュー指摘対応。[`Self::active_generation`] 参照）。
    generation: u64,
}

impl<'e> SessionTransaction<'e> {
    pub fn new(limits: TransactionLimits) -> Self {
        Self {
            state: TxnState::Idle,
            limits,
            generation: 0,
        }
    }

    /// 現在の状態（#943 が `ReadyForQuery` の状態バイトへ写像する想定の照会 API）。
    pub fn status(&self) -> TransactionStatus {
        match &self.state {
            TxnState::Idle => TransactionStatus::Idle,
            TxnState::Active(_) => TransactionStatus::InTransaction,
            TxnState::Failed { .. } => TransactionStatus::Failed,
        }
    }

    /// `Active` なら true（`core.rs` の文実行前チェック用）。
    pub fn is_active(&self) -> bool {
        matches!(self.state, TxnState::Active(_))
    }

    /// `Active` の間だけ、現在のトランザクションの世代番号を返す（PR #1049
    /// レビュー指摘対応）。wire-server 拡張クエリの portal が、カーソル
    /// `FETCH` の中断保持分（`PortalState::Suspended`）を束縛した時点の
    /// 世代と、再送出しようとしている時点の世代を突き合わせ、その間に
    /// `COMMIT`／`ROLLBACK`（→ `Idle`）や新しい `BEGIN` を挟んでいないかを
    /// 検証するために使う。`None`（`Idle`／`Failed`）はトランザクションが
    /// 既に終了しており、束縛が生きていた世代とは一致し得ないことを表す。
    pub fn active_generation(&self) -> Option<u64> {
        self.is_active().then_some(self.generation)
    }

    /// `BEGIN`。`Idle` から `Active` へ遷移する。`storage` から
    /// [`crate::storage::Storage::begin_explicit_write_txn`] で単一ライタの
    /// permit を取得したまま保持する（`writer_gate` モジュールドキュメント
    /// 参照。待機上限超過は `55P03`）。
    pub fn begin(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
    ) -> Result<(), SqlSurfaceError> {
        // 暗黙トランザクションはメッセージ内で完結する。明示 `BEGIN` が届くのは
        // 呼び出し側の不変条件違反（制御文を含むメッセージは暗黙にならない）なので、
        // 暗黙の `Active` は abort して `Idle` へ戻し、内部エラーで拒否する。
        if self.abort_if_implicit() {
            return Err(internal_error());
        }
        self.begin_with_origin(storage, session, TxnOrigin::Explicit)
    }

    /// 暗黙トランザクション（Issue #1175）を開始する。`BEGIN` を含まない複数文
    /// メッセージのうち、トランザクション外の書き込みが最後以外にある形を
    /// 原子的に実行するため、wire 層がメッセージの先頭で 1 回だけ呼ぶ
    /// （`EngineCore::begin_implicit_transaction` 経由）。
    ///
    /// `Idle` からだけ遷移できる。それ以外の状態は呼び出し側の不変条件違反
    /// （`Failed` のセッションは暗黙にせず個々の文で `25P02` を返す設計）なので
    /// `Internal`（`XX000`）で拒否し、状態は変えない。書き込みゲートの待機上限を
    /// 超えたら `55P03`。
    pub fn begin_implicit(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
    ) -> Result<(), SqlSurfaceError> {
        if !matches!(self.state, TxnState::Idle) {
            return Err(internal_error());
        }
        self.begin_with_origin(storage, session, TxnOrigin::Implicit)
    }

    /// 暗黙トランザクションの commit（Issue #1175）。最後の文の実行に成功した
    /// 直後に wire 層が呼ぶ。書き込みがあれば遅延 `FOREIGN KEY` の検査と
    /// [`crate::recovery::commit_boundary::commit`]（point of no return）を行い、
    /// 無ければ drop（abort）する。
    ///
    /// 成功・失敗のどちらでも `Idle` へ戻る（明示版の [`Self::commit`] は上限超過で
    /// `Failed` へ遷移するが、暗黙版はクライアントが `ROLLBACK` を発行できないため
    /// 遷移しない）。持続時間の上限超過は `54000`、commit・遅延制約違反の失敗は
    /// [`Self::commit`] と同じエラーで、いずれも `session` を開始時点へ復元する。
    /// 暗黙の `Active` 以外で呼ばれた場合は `Internal`（状態は変えない）。
    pub fn commit_implicit(&mut self, session: &mut SessionState) -> Result<(), SqlSurfaceError> {
        match std::mem::replace(&mut self.state, TxnState::Idle) {
            TxnState::Active(active) if active.origin == TxnOrigin::Implicit => {
                let ActiveTxn {
                    write_txn,
                    started_at,
                    has_writes,
                    session_at_begin,
                    written_by_tenant,
                    ..
                } = *active;
                if started_at.elapsed() > self.limits.max_duration {
                    drop(write_txn);
                    *session = session_at_begin;
                    return Err(SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE));
                }
                commit_active(
                    write_txn,
                    has_writes,
                    &written_by_tenant,
                    session_at_begin,
                    session,
                )
            }
            other => {
                self.state = other;
                Err(internal_error())
            }
        }
    }

    /// 暗黙トランザクションを破棄して `Idle` へ戻す（Issue #1175）。wire 層が
    /// 暗黙トランザクションを実行し終えた全経路の最後に呼ぶ後始末で、暗黙の
    /// `Active` 以外では何もしない（冪等。明示トランザクションには触れない）。
    pub fn abort_implicit(&mut self) {
        self.abort_if_implicit();
    }

    /// 暗黙トランザクションの `Active` なら true（Issue #1175）。wire 層が、各文の
    /// 実行後にトランザクションがまだ暗黙のまま生きているか（後続の書き込みが
    /// autocommit で実行されないか）を確認するために使う。
    pub fn is_implicit_active(&self) -> bool {
        matches!(&self.state, TxnState::Active(a) if a.origin == TxnOrigin::Implicit)
    }

    /// 暗黙の `Active` なら write txn を abort して `Idle` へ戻し true を返す。
    fn abort_if_implicit(&mut self) -> bool {
        if !self.is_implicit_active() {
            return false;
        }
        // `write_txn` は drop（abort）され、内包する permit はその後に解放される。
        self.state = TxnState::Idle;
        true
    }

    fn begin_with_origin(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
        origin: TxnOrigin,
    ) -> Result<(), SqlSurfaceError> {
        match &self.state {
            TxnState::Idle => {
                let write_txn = storage
                    .begin_explicit_write_txn()
                    .map_err(map_write_lock_err)?;
                // 世代番号を先に進めてから `Active` へ遷移する（PR #1049
                // レビュー指摘対応。オーバーフロー目前で頭打ちにし、万一
                // 到達しても既存の世代番号を使い回して衝突させることはない
                // ―― 到達し得ない前提の防御的縮退のため専用の `wire_code`
                // は設けず、`Internal` として拒否する）。`write_txn` を
                // 閉包へ move させないよう、先に `match` で判定してから
                // 代入する（`ok_or_else` クロージャは呼ばれない場合でも
                // 構築時に move が確定するため使えない）。
                let next_generation = match self.generation.checked_add(1) {
                    Some(g) => g,
                    None => {
                        drop(write_txn);
                        return Err(SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        });
                    }
                };
                self.generation = next_generation;
                self.state = TxnState::Active(Box::new(ActiveTxn {
                    origin,
                    write_txn,
                    started_at: Instant::now(),
                    statements: 0,
                    seen_operation_ids: HashSet::new(),
                    written_tables: HashSet::new(),
                    written_by_tenant: std::collections::BTreeSet::new(),
                    has_writes: false,
                    session_at_begin: session.clone(),
                    cursors: crate::sql::cursor::CursorRegistry::new(),
                    _marker: std::marker::PhantomData,
                }));
                Ok(())
            }
            TxnState::Active(_) => {
                // 入れ子の BEGIN はトランザクションを Failed へ遷移させる
                // （PostgreSQL の "WARNING" 相当ではなく fail-closed に倒す。
                // `docs/design/explicit-transaction.md` 参照）。
                self.fail();
                Err(SqlSurfaceError::ActiveSqlTransaction)
            }
            TxnState::Failed { .. } => Err(self.take_failed_error()),
        }
    }

    /// `COMMIT`。`Active` なら書き込みの有無に応じて commit／abort し `Idle` へ
    /// 遷移する。`Idle`／`Failed` はそれぞれ `25P01`／`25P02` を返す（`Failed`
    /// のままにし、`ROLLBACK` のみを受理し続ける）。
    ///
    /// - 持続時間の上限を超えている場合は確定させず、abort して `Failed` へ遷移し
    ///   `54000` を返す（文実行時の上限超過と同じ契約。PR #1041 レビュー指摘）。
    /// - 書き込みがあれば、commit の**前**に `written_by_tenant`（このトランザクション
    ///   内で書き込んだ `(tenant, table)` の集合）の各要素について、文単位検査から
    ///   先送りしていた `INITIALLY DEFERRED` の `FOREIGN KEY` を検査する
    ///   （[`crate::constraint::enforce_deferred_foreign_keys_in_txn`]。TABLE-17・
    ///   TASK-205、Issue #1077）。違反すれば abort し、`session` を `BEGIN` 時点の
    ///   状態へ復元してから `Idle` へ戻り `23503` を返す（PostgreSQL と同じく
    ///   ロールバック扱い。commit 自体の失敗〔下記〕とは別の分岐として扱う）。
    /// - commit 自体が失敗した場合は、PostgreSQL と同じくロールバック扱いとし、
    ///   `session` を `BEGIN` 時点の状態へ復元してから `Idle` へ戻る。
    pub fn commit(&mut self, session: &mut SessionState) -> Result<(), SqlSurfaceError> {
        if self.abort_if_implicit() {
            return Err(internal_error());
        }
        match std::mem::replace(&mut self.state, TxnState::Idle) {
            TxnState::Idle => {
                self.state = TxnState::Idle;
                Err(SqlSurfaceError::NoActiveSqlTransaction)
            }
            TxnState::Active(active) => {
                let ActiveTxn {
                    write_txn,
                    started_at,
                    has_writes,
                    session_at_begin,
                    written_by_tenant,
                    ..
                } = *active;
                if started_at.elapsed() > self.limits.max_duration {
                    // `write_txn` を drop（abort）し、内包する permit を解放する。
                    drop(write_txn);
                    self.state = TxnState::Failed {
                        session_at_begin,
                        expired: false,
                    };
                    return Err(SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE));
                }
                // 状態は冒頭の `mem::replace` で既に `Idle`。成否によらず `Idle`。
                commit_active(
                    write_txn,
                    has_writes,
                    &written_by_tenant,
                    session_at_begin,
                    session,
                )
            }
            TxnState::Failed {
                session_at_begin,
                expired,
            } => {
                // `Failed` のまま据え置く。`BEGIN` 時点の `SessionState` は後続の
                // `ROLLBACK` が復元に使うため、取り出したものをそのまま戻す
                // （既定値で上書きすると `SET search_mode`・`CREATE FUNCTION` 等の
                // BEGIN 前のセッション状態が失われる）。
                self.state = TxnState::Failed {
                    session_at_begin,
                    expired,
                };
                Err(self.take_failed_error())
            }
        }
    }

    /// `ROLLBACK`。`Active`/`Failed` いずれからも `Idle` へ戻り、`BEGIN` 時点の
    /// `SessionState`（`SET search_mode`・`CREATE FUNCTION` 等）を復元する。
    /// `Idle` からの `ROLLBACK` は `25P01`。
    pub fn rollback(&mut self, session: &mut SessionState) -> Result<(), SqlSurfaceError> {
        if self.abort_if_implicit() {
            return Err(internal_error());
        }
        match std::mem::replace(&mut self.state, TxnState::Idle) {
            TxnState::Idle => {
                self.state = TxnState::Idle;
                Err(SqlSurfaceError::NoActiveSqlTransaction)
            }
            TxnState::Active(active) => {
                // `write_txn` は先に drop（abort）してから `permit` を解放する
                // （構造体のフィールド宣言順どおりの drop 順序。writer_gate
                // モジュールドキュメント参照）。
                *session = active.session_at_begin;
                self.state = TxnState::Idle;
                Ok(())
            }
            TxnState::Failed {
                session_at_begin, ..
            } => {
                *session = session_at_begin;
                self.state = TxnState::Idle;
                Ok(())
            }
        }
    }

    /// 文実行前チェック（`Active` のときのみ意味を持つ）。上限超過・
    /// `operation_id` の同一トランザクション内再利用を検出し、検出時は
    /// トランザクションを `Failed` へ遷移させる。
    pub(crate) fn check_and_register_statement(
        &mut self,
        operation_id: Option<&str>,
    ) -> Result<(), SqlSurfaceError> {
        let active = match &mut self.state {
            TxnState::Active(active) => active,
            _ => return Ok(()),
        };
        if active.started_at.elapsed() > self.limits.max_duration
            || active.statements >= self.limits.max_statements
        {
            self.fail();
            return Err(SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE));
        }
        if let Some(id) = operation_id {
            if !active.seen_operation_ids.insert(id.to_string()) {
                self.fail();
                return Err(SqlSurfaceError::InvalidTransactionState);
            }
        }
        active.statements = active.statements.saturating_add(1);
        Ok(())
    }

    /// 対象テーブルへの書き込みを記録する（読み取りの `written_tables` 判定用に
    /// 加え、`tenant_id` を伴う `written_by_tenant` へも記録する。TABLE-17・
    /// TASK-205、Issue #1077。COMMIT 時の遅延 `FOREIGN KEY` 検査の走査範囲は
    /// この記録だけから導出するため、呼び出し元（`core.rs` の INSERT・
    /// TRUNCATE）は書き込みのたびに必ず呼ぶこと——省くと fail-open になる）。
    pub(crate) fn mark_written(&mut self, tenant_id: &str, table: &str) {
        if let TxnState::Active(active) = &mut self.state {
            active.written_tables.insert(table.to_string());
            active
                .written_by_tenant
                .insert((tenant_id.to_string(), table.to_string()));
            active.has_writes = true;
        }
    }

    /// `table` が同一トランザクション内で既に書き込み済みかどうか（読み取り時の
    /// `0A000` 判定に使う。§2.5 の既知の逸脱）。
    pub(crate) fn table_already_written(&self, table: &str) -> bool {
        match &self.state {
            TxnState::Active(active) => active.written_tables.contains(table),
            _ => false,
        }
    }

    /// 現在保持している共有 `redb::WriteTransaction`（`Active` のときのみ
    /// `Some`）。書き込み系の実行本体が [`crate::tenant::WriteTarget::InTxn`]
    /// を構築するために使う。
    pub(crate) fn write_txn(&self) -> Option<&redb::WriteTransaction> {
        match &self.state {
            TxnState::Active(active) => Some(&*active.write_txn),
            _ => None,
        }
    }

    /// `Active` のときのみ、カーソル集合への可変参照を返す（`DECLARE`/
    /// `FETCH`/`CLOSE` の実行本体〔`core.rs::EngineCore::
    /// execute_cursor_in_active_txn`〕が使う）。
    pub(crate) fn cursors_mut(&mut self) -> Option<&mut crate::sql::cursor::CursorRegistry> {
        match &mut self.state {
            TxnState::Active(active) => Some(&mut active.cursors),
            _ => None,
        }
    }

    /// `name` のカーソルが `Active` なトランザクション内に存在すれば、その
    /// 結果列メタデータを返す（Describe 専用の読み取り専用アクセサ。
    /// `core.rs::EngineCore::describe_parsed_in_txn` が使う。検索本体・
    /// カーソルの取得位置には一切触れない）。
    pub(crate) fn cursor_columns(&self, name: &str) -> Option<Vec<crate::sql::exec::ColumnMeta>> {
        match &self.state {
            TxnState::Active(active) => active.cursors.columns(name),
            _ => None,
        }
    }

    /// `name` のカーソルが `Active` なトランザクション内に存在すれば、その
    /// `DECLARE` インスタンス固有の識別子を返す（PR #1049 レビュー指摘対応。
    /// `pub` — `wire-server::extended_query` が拡張クエリの portal 束縛検証
    /// から直接呼ぶ。`Idle`／`Failed`、またはカーソルが `CLOSE` 済み・
    /// 再 `DECLARE` された別インスタンスであれば `None`／不一致となり、
    /// 呼び出し元はその中断保持分の再送出を拒否する契約。`docs/design/
    /// sql-cursor.md`「wire-server 拡張クエリの portal 束縛」節参照）。
    pub fn cursor_id(&self, name: &str) -> Option<u64> {
        match &self.state {
            TxnState::Active(active) => active.cursors.cursor_id(name),
            _ => None,
        }
    }

    /// 任意のエラーで `Active` → `Failed` へ強制遷移させる（文実行中に
    /// エラーが起きた場合、`core.rs` の実行ディスパッチが呼ぶ）。
    ///
    /// `Active` 以外（`Idle`・`Failed`）では状態を一切変えない（冪等）。wire 層は
    /// エラー応答のたびに状態を問わず呼ぶため、`Failed` を `Idle` へ戻したり
    /// `session_at_begin` を失ったりしてはならない。
    pub fn fail(&mut self) {
        if !matches!(self.state, TxnState::Active(_)) {
            return;
        }
        // 暗黙トランザクション（Issue #1175）は `Failed` を経由せず `Idle` へ戻す
        // （クライアントが `ROLLBACK` を発行できないため）。セッション状態の復元は
        // wire 層がメッセージ開始時のスナップショットで行う。
        if self.abort_if_implicit() {
            return;
        }
        if let TxnState::Active(active) = std::mem::replace(&mut self.state, TxnState::Idle) {
            // `write_txn` は drop（abort）され、`permit` はその後に解放される。
            self.state = TxnState::Failed {
                session_at_begin: active.session_at_begin,
                expired: false,
            };
        }
    }

    /// 持続時間の上限を過ぎた `Active` なトランザクションを `Failed` へ遷移させ、
    /// 共有書き込みトランザクションを abort してライタを解放する（遷移したら
    /// `true`）。wire 層が要求を受け取るたびに文の種類を問わず呼ぶ（Sync・Flush 等
    /// SQL を伴わない要求だけを送り続けてライタを保持し続けることを防ぐ。PR #1041
    /// レビュー指摘）。遷移後の最初の文／`COMMIT` には、文実行時の上限超過と同じ
    /// `54000` を返す（[`Self::take_failed_error`]）。
    pub fn release_if_expired(&mut self) -> bool {
        let expired = match &self.state {
            TxnState::Active(active) => active.started_at.elapsed() > self.limits.max_duration,
            _ => false,
        };
        if !expired {
            return false;
        }
        // 暗黙トランザクションはメッセージをまたがないため通常は到達しない。
        // 防御として abort して `Idle` へ戻す（`Failed` にはしない）。
        if self.abort_if_implicit() {
            return true;
        }
        if let TxnState::Active(active) = std::mem::replace(&mut self.state, TxnState::Idle) {
            self.state = TxnState::Failed {
                session_at_begin: active.session_at_begin,
                expired: true,
            };
        }
        true
    }

    /// `Failed` 中に `ROLLBACK` 以外の文を受け取ったときに返すエラー。
    /// [`Self::release_if_expired`] による遷移をまだ報告していなければ `54000`
    /// （上限超過）を 1 回だけ返し、それ以外は `25P02` を返す。
    pub fn take_failed_error(&mut self) -> SqlSurfaceError {
        if let TxnState::Failed { expired, .. } = &mut self.state {
            if *expired {
                *expired = false;
                return SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE);
            }
        }
        SqlSurfaceError::InFailedSqlTransaction
    }

    /// `Active` なトランザクションが持続時間の上限に達するまでの残り時間
    /// （`Active` 以外は `None`。上限を過ぎていれば `Some(Duration::ZERO)`）。
    ///
    /// wire 層（`wire-server::handshake::post_auth_loop`）が、次の要求を待つ
    /// 読み取りタイムアウトをこの残り時間で切り詰めるために使う（PR #1041
    /// レビュー指摘: 無通信のまま上限を過ぎてもライタを占有し続けないよう、
    /// 期限到達時点で [`Self::release_if_expired`] を呼べるようにする）。
    /// `Failed` は [`Self::fail`]／[`Self::release_if_expired`] の時点で共有
    /// 書き込みトランザクション（書き込みゲートの permit を内包）を既に drop
    /// 済みでライタを保持しないため、対象外（`None`）とする。
    pub fn remaining_duration(&self) -> Option<Duration> {
        match &self.state {
            TxnState::Active(active) => Some(
                self.limits
                    .max_duration
                    .saturating_sub(active.started_at.elapsed()),
            ),
            _ => None,
        }
    }
}

/// 内部エラー（`XX000`）。他テナントの情報を含まない固定文言。
fn internal_error() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// `Active` から取り出した共有書き込みトランザクションを確定する共通本体
/// （明示 [`SessionTransaction::commit`]・暗黙 [`SessionTransaction::commit_implicit`]
/// が共有する。遅延制約検査と commit の順序・ロールバック扱いの契約を 1 箇所に保つ）。
///
/// 書き込みがあれば、commit の**前**に `written_by_tenant` の各要素について遅延
/// `FOREIGN KEY` を検査し（TABLE-17・TASK-205、Issue #1077）、違反すれば abort して
/// `session` を開始時点へ復元し `23503` を返す。commit 自体の失敗も同様にロールバック
/// 扱いで `XX000`。呼び出し元は事前に `state` を `Idle` にしてあること。
fn commit_active(
    write_txn: crate::storage::GatedWriteTxn,
    has_writes: bool,
    written_by_tenant: &std::collections::BTreeSet<(String, String)>,
    session_at_begin: SessionState,
    session: &mut SessionState,
) -> Result<(), SqlSurfaceError> {
    if has_writes {
        for (tenant_id, table) in written_by_tenant {
            if let Err(e) = crate::constraint::enforce_deferred_foreign_keys_in_txn(
                &write_txn, tenant_id, table,
            ) {
                // `write_txn` を drop（abort）し permit を解放したうえで開始時点の
                // セッション状態を復元する（commit 自体の失敗と同じロールバック扱い）。
                drop(write_txn);
                *session = session_at_begin;
                return Err(crate::sql::exec::map_write_error(e, "commit"));
            }
        }
    }
    let result = if has_writes {
        crate::recovery::commit_boundary::commit(write_txn)
    } else {
        drop(write_txn);
        Ok(())
    };
    result.map_err(|_| {
        // commit 失敗はロールバック扱い（`write_txn` は消費済みで、permit も
        // 解放されている）。
        *session = session_at_begin;
        internal_error()
    })
}

/// [`crate::storage::StorageError::WriteLockTimeout`]／
/// [`crate::storage::StorageError::WriteTxnHeldByCurrentSession`] を
/// `SqlSurfaceError::LockNotAvailable`（`55P03`）へ写像する。それ以外は
/// 内部事象として `XX000` に丸める（他テナントの情報を含まない）。
fn map_write_lock_err(e: crate::storage::StorageError) -> SqlSurfaceError {
    match e {
        crate::storage::StorageError::WriteLockTimeout
        | crate::storage::StorageError::WriteTxnHeldByCurrentSession => {
            SqlSurfaceError::LockNotAvailable
        }
        _ => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_of(sql: &str) -> Vec<Token> {
        crate::sql::lexer::tokenize(sql).expect("tokenize")
    }

    #[test]
    fn begin_variants_are_accepted() {
        for sql in ["BEGIN", "BEGIN WORK", "BEGIN TRANSACTION", "begin"] {
            let ctrl = validate_transaction_control_tokens(&tokens_of(sql)).expect("accepted");
            assert_eq!(ctrl, TxnControl::Begin);
        }
    }

    #[test]
    fn commit_and_rollback_variants_are_accepted() {
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("COMMIT")).unwrap(),
            TxnControl::Commit
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("COMMIT WORK")).unwrap(),
            TxnControl::Commit
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("ROLLBACK")).unwrap(),
            TxnControl::Rollback
        );
        assert_eq!(
            validate_transaction_control_tokens(&tokens_of("ROLLBACK TRANSACTION")).unwrap(),
            TxnControl::Rollback
        );
    }

    #[test]
    fn trailing_clauses_are_rejected() {
        for sql in [
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "BEGIN READ ONLY",
            "COMMIT AND CHAIN",
            "ROLLBACK TO SAVEPOINT x",
        ] {
            let err = validate_transaction_control_tokens(&tokens_of(sql)).unwrap_err();
            assert_eq!(err.wire_code(), "42601");
        }
    }

    /// `COMMIT` の commit 自体が失敗した場合は PostgreSQL と同じくロールバック
    /// 扱いとし、`BEGIN` 時点の `SessionState` を復元してから `Idle` へ戻る
    /// （PR #1041 レビュー指摘）。全体世代カウンタを上限値にしておき、commit 直前の
    /// 世代加算（`storage::prepare_generation_bump`）をオーバーフローで失敗させる。
    #[test]
    fn commit_failure_restores_session_state_at_begin_and_returns_to_idle() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::sql::mode::SearchMode;
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("sql31-commit-failure");
        let _cleanup = CleanupGuard(path.clone());
        let storage = crate::storage::Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "documents",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(crate::storage::GENERATION_TABLE)
                    .expect("open generation table");
                table
                    .insert("generation", u64::MAX)
                    .expect("set generation");
            }
            write_txn.commit().expect("commit generation");
        }
        let engine = crate::core::EngineCore::from_storage(
            storage,
            Box::new(crate::kernel::CpuScalarProvider),
        );
        let caller = crate::policy::PolicyContext::with_visibilities(
            "tenant-a",
            [
                crate::storage::Visibility::Public,
                crate::storage::Visibility::Private,
            ],
        )
        .expect("valid tenant");
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();

        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "SET search_mode = 'precision'",
            )
            .expect("set search_mode before begin");
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "SET search_mode = 'recall'",
            )
            .expect("set search_mode inside transaction");
        engine
            .execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "INSERT INTO documents (id, embedding) VALUES (1, '[1.0, 0.0]') \
                 USING OPERATION_ID 'op-commit-failure'",
            )
            .expect("insert");
        assert_eq!(session.search_mode(), Some(SearchMode::Recall));

        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
            .expect_err("commit fails on generation overflow");
        assert_eq!(txn.status(), TransactionStatus::Idle);
        assert_eq!(
            session.search_mode(),
            Some(SearchMode::Precision),
            "a failed COMMIT must behave as ROLLBACK and restore the session state"
        );
    }

    #[test]
    fn begin_then_commit_round_trips_through_idle() {
        let limits = TransactionLimits::default();
        let mut txn: SessionTransaction<'static> = SessionTransaction::new(limits);
        assert_eq!(txn.status(), TransactionStatus::Idle);
        // `begin`/`commit`/`rollback` 自体の redb 結線は `core.rs`・wire-server
        // 側の結合テストで検証する（本テストは構文受理のみを対象とする）。
        let _ = &mut txn;
    }
}
