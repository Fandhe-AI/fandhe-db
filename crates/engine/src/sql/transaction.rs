//! 明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`（SQL-31・TASK-221）の構文
//! 受理と、セッション単位の状態機械 [`SessionTransaction`] を提供する。
//!
//! `wire-server` の簡易クエリ（`simple_query.rs`）・拡張クエリ
//! （`extended_query.rs`）の両経路が、接続（セッション）ごとに 1 つの
//! [`SessionTransaction`] を保持し、`core::EngineCore::execute_sql_in_txn`／
//! `execute_parsed_in_txn` へ `&mut` で渡す。`Idle`（トランザクション外）の間は
//! 既存の autocommit 経路（[`crate::core::EngineCore::execute_sql_in_session`]）と
//! ビット同一に振る舞い、`Active` の間は `BEGIN` 時点の読み取りスナップショットと、
//! 共有 `redb::WriteTransaction`（[`crate::storage::Storage::begin_explicit_write_txn`]。
//! 取得済み／未取得を `Option` で表し、現状は `BEGIN` で取得する）を保持して複数の
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
//! 読み取り（Issue #1179）: 自トランザクションが未 commit の変更を持たない間は
//! BEGIN 時点の確定済みスナップショットで従来どおり読む（キャッシュ利用可）。変更を
//! 持つ間（[`SessionTransaction::dirty_tables`] が非空）は、共有書き込み
//! トランザクションを読み取り源（`storage::read_source::ReadSource`）にして同じ
//! 実行本体で読み、未 commit の変更を反映する（テーブル世代キーのキャッシュは使わない）。
//! `USING PLAN` の検索 SELECT・`EXPLAIN`・ファイル形 `INSERT` も受け付ける（Issue #1353。
//! カタログは確定済み、辞書と行の読みは共有書き込みトランザクションから。辞書はキャッシュしない）。
//! 詳細は `docs/design/explicit-transaction.md` 参照。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{Keyword, Token};
use crate::sql::mode::SessionState;

/// `BEGIN`/`COMMIT`/`ROLLBACK`（別名 `START TRANSACTION`/`END`/`ABORT`。Issue #1930）の
/// 種別（構造検証のみ。実行は [`SessionTransaction`] が担う）。`StartTransaction` は
/// `Begin` と状態遷移が同一で、CommandComplete タグだけが異なる（`START TRANSACTION`）。
/// `END` は `Commit`、`ABORT` は `Rollback` へ写像する。
///
/// **BREAKING CHANGE**（Issue #1930）: variant `StartTransaction` を追加した。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnControl {
    Begin,
    StartTransaction,
    Commit,
    Rollback,
}

fn ident_is(tok: Option<&Token>, word: &str) -> bool {
    matches!(tok, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(word))
}

/// 先頭トークン列がトランザクション制御文（別名・`PREPARE TRANSACTION` を含む）かどうか。
/// `core.rs` の parse 入口・一時表経路のトークン走査免除が共通で使う（SQL-31・SQL-69 (1)）。
/// 受理可否の検証は [`validate_transaction_control_tokens`] が担う。
pub(crate) fn is_transaction_control_head(tokens: &[Token]) -> bool {
    let first = tokens.first();
    ["BEGIN", "COMMIT", "ROLLBACK", "END", "ABORT"]
        .iter()
        .any(|w| ident_is(first, w))
        || ((ident_is(first, "START") || ident_is(first, "PREPARE"))
            && ident_is(tokens.get(1), "TRANSACTION"))
}

/// トランザクションのアクセスモード（SQL-38・Issue #1932）。`BEGIN READ ONLY`／
/// `BEGIN READ WRITE` の修飾子を表し、[`ActiveTxn`] が保持する。読み取り専用の実体
/// （読み取り Tx で開く・書き込みの `25006`）は #1933／#1934 の範囲で、それまで
/// `ReadOnly` は [`SessionTransaction::begin_with_mode`] が `0A000` で拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnAccessMode {
    ReadWrite,
    ReadOnly,
}

/// [`validate_transaction_control_tokens`] の検証結果。`access_mode` は `BEGIN` で
/// 明示されたときだけ `Some`（`COMMIT`／`ROLLBACK` は常に `None`）。`None` を残すのは
/// 将来のセッション既定値（#1939）を適用する余地のため。`core.rs` の
/// `ParsedSql::Transaction` が保持し、実行時に `begin_with_mode` へ渡される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedTxnControl {
    pub control: TxnControl,
    pub access_mode: Option<TxnAccessMode>,
}

fn unsupported_txn_mode() -> SqlSurfaceError {
    SqlSurfaceError::unsupported("unsupported transaction mode")
}

/// `BEGIN` のモード句 1 項目（`READ ONLY`／`READ WRITE`）を読み、残りのトークンを返す。
/// 未知の語・単独の `READ` は `42601`（fail-closed）。#1938 は ISOLATION LEVEL をここへ足す。
fn parse_txn_mode_item(rest: &[Token]) -> Result<(TxnAccessMode, &[Token]), SqlSurfaceError> {
    let word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Ident(x)) if x.eq_ignore_ascii_case(w));
    if !word(rest.first(), "READ") {
        return Err(unsupported_txn_mode());
    }
    let after_read = rest.split_first().map_or(&[][..], |(_, t)| t);
    let mode = if word(after_read.first(), "ONLY") {
        TxnAccessMode::ReadOnly
    } else if word(after_read.first(), "WRITE") {
        TxnAccessMode::ReadWrite
    } else {
        return Err(unsupported_txn_mode());
    };
    let tail = after_read.split_first().map_or(&[][..], |(_, t)| t);
    Ok((mode, tail))
}

/// トランザクション制御文の規範形を受理する。`BEGIN`/`START TRANSACTION`/`COMMIT`/`END`/
/// `ROLLBACK`/`ABORT`（`START TRANSACTION` 以外は `[WORK|TRANSACTION]` 修飾語を許容し、
/// `BEGIN` に限りアクセスモード `READ ONLY`／`READ WRITE` を受理）。
/// `AND [NO] CHAIN` と `PREPARE TRANSACTION` は `0A000`、分離レベル指定・`DEFERRABLE`・
/// `$n`・重複／矛盾するモード等その他の後続句は `42601`（fail-closed）。先頭が制御文の語で
/// あることは呼び出し元（[`is_transaction_control_head`]）が確認済みの前提。
pub(crate) fn validate_transaction_control_tokens(
    tokens: &[Token],
) -> Result<ValidatedTxnControl, SqlSurfaceError> {
    let Some((Token::Ident(head), tail)) = tokens.split_first() else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };
    if head.eq_ignore_ascii_case("PREPARE") {
        return Err(SqlSurfaceError::feature_not_supported(
            "PREPARE TRANSACTION is not supported",
        ));
    }
    // 受信 SQL 由来のトークン列のため添字スライスを使わず `split_first` で進める。
    if head.eq_ignore_ascii_case("START") {
        return match tail.split_first() {
            Some((Token::Ident(kw), rest)) if kw.eq_ignore_ascii_case("TRANSACTION") => {
                if rest.is_empty() {
                    Ok(ValidatedTxnControl {
                        control: TxnControl::StartTransaction,
                        access_mode: None,
                    })
                } else {
                    Err(SqlSurfaceError::unsupported(
                        "unsupported clause after transaction control statement",
                    ))
                }
            }
            _ => Err(SqlSurfaceError::unsupported(
                "malformed transaction control statement",
            )),
        };
    }
    let ctrl = if head.eq_ignore_ascii_case("BEGIN") {
        TxnControl::Begin
    } else if head.eq_ignore_ascii_case("COMMIT") || head.eq_ignore_ascii_case("END") {
        TxnControl::Commit
    } else if head.eq_ignore_ascii_case("ROLLBACK") || head.eq_ignore_ascii_case("ABORT") {
        TxnControl::Rollback
    } else {
        return Err(SqlSurfaceError::unsupported(
            "malformed transaction control statement",
        ));
    };

    let mut rest = match tail.split_first() {
        Some((Token::Ident(kw), t))
            if kw.eq_ignore_ascii_case("WORK") || kw.eq_ignore_ascii_case("TRANSACTION") =>
        {
            t
        }
        _ => tail,
    };
    if ctrl != TxnControl::Begin && matches!(rest.first(), Some(Token::Keyword(Keyword::And))) {
        let after = rest.get(1);
        let chain =
            ident_is(after, "CHAIN") || (ident_is(after, "NO") && ident_is(rest.get(2), "CHAIN"));
        if chain {
            return Err(SqlSurfaceError::feature_not_supported(
                "AND CHAIN is not supported",
            ));
        }
    }
    let mut access_mode = None;
    if ctrl == TxnControl::Begin {
        // モード句ループ。2 項目目以降は任意の `,` で区切れる構造にしてある（#1938 向け）が、
        // 現状は項目がアクセスモード 1 つのため、重複・矛盾は `42601`。トークン数で有界。
        let mut first = true;
        while !rest.is_empty() {
            if !first {
                if let Some((Token::Punct(','), tail)) = rest.split_first() {
                    rest = tail;
                }
            }
            first = false;
            let (mode, tail) = parse_txn_mode_item(rest)?;
            if access_mode.replace(mode).is_some() {
                return Err(unsupported_txn_mode());
            }
            rest = tail;
        }
    }
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after transaction control statement",
        ));
    }
    Ok(ValidatedTxnControl {
        control: ctrl,
        access_mode,
    })
}

/// SQL テキストの先頭トークンが `ROLLBACK`／`ABORT`（大小無視）かどうか。明示トランザクションが
/// `Failed` の間に受理してよい文かを parse 前に判定するために使う（SQL-31・TASK-221。
/// `core::EngineCore::execute_sql_in_txn`、wire-server の拡張クエリ Parse が呼ぶ）。
/// `END`・`COMMIT` は false（失敗状態では `25P02` で拒否する）。規範形の検証は通常どおり
/// parse に任せる。字句解析に失敗した入力は `false`（fail-closed）。
pub fn is_rollback_statement(sql: &str) -> bool {
    matches!(
        crate::sql::lexer::tokenize(sql).ok().as_deref().and_then(|t| t.first()),
        Some(Token::Ident(name))
            if name.eq_ignore_ascii_case("ROLLBACK") || name.eq_ignore_ascii_case("ABORT")
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

/// トランザクション中の状態。`BEGIN` 時点の読み取りスナップショット（`read_snapshot`）と、
/// 取得済み／未取得を表せる共有書き込みトランザクション（`writer`）、およびその付随状態を持つ
/// （SQL-31・PERSIST-4・TASK-333、Issue #2399。設計記録は `docs/design/explicit-transaction.md`）。
/// `writer` の中身（[`crate::storage::GatedWriteTxn`]）は書き込みゲートの permit を内包し、
/// drop 時は abort してから permit を解放する。フィールドの宣言順が drop 順で、`writer` を
/// `read_snapshot` より先に置いて permit の解放を遅らせない。
struct ActiveTxn<'e> {
    origin: TxnOrigin,
    /// アクセスモード（SQL-38・Issue #1932）。#1933／#1934 が読み取り Tx の開始と
    /// 書き込み拒否の判定に使う。現状 `ReadOnly` は開始前に拒否されるため常に `ReadWrite`。
    access_mode: TxnAccessMode,
    /// 共有書き込みトランザクション。`None` は writer 未取得（後続 Issue #2400 以降に生じる
    /// 状態）。本 Issue の時点では `BEGIN`（明示・暗黙とも）で必ず取得するため、`Active` の間は
    /// 常に `Some`。
    writer: Option<crate::storage::GatedWriteTxn>,
    /// `BEGIN` 時点の確定済み読み取りスナップショット。writer 取得の後に開くため、文ごとの
    /// 読み取りが見る確定状態と同じ。本 Issue ではどの読み取り経路にも結線しない（結線は
    /// #2400・#2403）。`ActiveTxn` を捨てる全経路で自動的に解放される。
    #[allow(dead_code)]
    read_snapshot: redb::ReadTransaction,
    /// 一時表（TABLE-44・Issue #1684）の書き込みトランザクション。最初の一時表の文で遅延して開く。
    /// `ActiveTxn` を取り出して drop する全経路（`rollback`／`fail`／`release_if_expired`／
    /// 暗黙の abort／`SessionTransaction` の drop）で自動的に abort する。commit は
    /// [`commit_active`] が永続側の commit 成功後にだけ行う。
    temp: Option<TempTxn>,
    started_at: Instant,
    /// トランザクション開始時刻（UTC マイクロ秒の壁時計。SQL-37・Issue #1586）。`now()` 系の
    /// 値で、トランザクション内の全文で固定する（`started_at` は壁時計でないため別に持つ）。
    txn_start_micros: i64,
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
    /// dirty テーブル判定（[`SessionTransaction::dirty_tables`]）が確定済み世代を
    /// 読むための参照。`Active` の間だけ保持する（`begin` が渡す）。
    storage: &'e crate::storage::Storage,
}

/// 開いている一時側の書き込みトランザクションと、その実体ストア（TABLE-44・Issue #1684）。
/// フィールドの宣言順が drop 順で、トランザクションをストアより先に破棄する。
pub(crate) struct TempTxn {
    txn: crate::storage::GatedWriteTxn,
    store: std::sync::Arc<crate::sql::temp_rows::TempRowStore>,
}

impl TempTxn {
    /// 一時側を確定する。永続側の commit 成功後にだけ呼ぶ。
    fn commit(self) -> Result<(), SqlSurfaceError> {
        let Self { txn, store } = self;
        store.commit(txn)
    }
}

enum TxnState<'e> {
    Idle,
    Active(Box<ActiveTxn<'e>>),
    /// `expired` は、持続時間の上限超過（[`SessionTransaction::release_if_expired`]）
    /// で `Failed` へ遷移し、まだ `54000` をクライアントへ報告していないことを表す。
    Failed {
        /// 巻き戻し用の `BEGIN` 時点のセッション。`SessionState` が育ったため `Active` と同程度の大きさに
        /// 揃えるよう `Box` で持つ（enum の variant 間のサイズ差を抑える）。
        session_at_begin: Box<SessionState>,
        expired: bool,
    },
}

/// 持続時間・文数の上限超過時のエラー文言（`54000`）。
const LIMIT_EXCEEDED_MESSAGE: &str = "transaction exceeded duration or statement limit";

/// 明示トランザクションの最大持続時間（ミリ秒）の下限（Issue #1553・SQL-46 のポインタ）。
pub const MIN_TRANSACTION_DURATION_MS: u64 = 100;
/// 最大持続時間（ミリ秒）の既定値（`TransactionLimits::default()` と同値）。
pub const DEFAULT_TRANSACTION_DURATION_MS: u64 = 20_000;
/// 最大持続時間（ミリ秒）のハード上限。単一ライタを握る時間の上限として、書き込みロック
/// 待ち上限の既定（30 秒）の 2 倍を暫定採用する（オーナー確認待ち。
/// `docs/design/transaction-limits-cli.md`）。
pub const MAX_TRANSACTION_DURATION_MS: u64 = 60_000;

/// 最大文数の下限。
pub const MIN_TRANSACTION_STATEMENTS: u32 = 1;
/// 最大文数の既定値（`TransactionLimits::default()` と同値）。
pub const DEFAULT_TRANSACTION_STATEMENTS: u32 = 1_000;
/// 最大文数のハード上限（既定の 10 倍。暫定・オーナー確認待ち）。
pub const MAX_TRANSACTION_STATEMENTS: u32 = 10_000;

/// トランザクション上限設定値が範囲外であることを表すエラー（Issue #1553）。
/// 文言は項目名・入力値・範囲だけを含む（テナントやデータの情報は含めない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionLimitOutOfRange {
    pub item: &'static str,
    pub value: u64,
    pub min: u64,
    pub max: u64,
}

impl std::fmt::Display for TransactionLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} value {} is out of range {}..={}",
            self.item, self.value, self.min, self.max
        )
    }
}

impl std::error::Error for TransactionLimitOutOfRange {}

/// 最大持続時間（ミリ秒）を検証して [`Duration`] にする。範囲検証の単一情報源
/// （wire-server の `transaction_limits_opt` が CLI 文字列を厳密パースして呼ぶ）。
pub fn validate_max_transaction_duration_ms(
    value: u64,
) -> Result<Duration, TransactionLimitOutOfRange> {
    if (MIN_TRANSACTION_DURATION_MS..=MAX_TRANSACTION_DURATION_MS).contains(&value) {
        Ok(Duration::from_millis(value))
    } else {
        Err(TransactionLimitOutOfRange {
            item: "max transaction duration (ms)",
            value,
            min: MIN_TRANSACTION_DURATION_MS,
            max: MAX_TRANSACTION_DURATION_MS,
        })
    }
}

/// 最大文数を検証して `u32` にする（範囲検証の単一情報源。`u32` に収まらない値も範囲外）。
pub fn validate_max_transaction_statements(value: u64) -> Result<u32, TransactionLimitOutOfRange> {
    let out_of_range = || TransactionLimitOutOfRange {
        item: "max transaction statements",
        value,
        min: u64::from(MIN_TRANSACTION_STATEMENTS),
        max: u64::from(MAX_TRANSACTION_STATEMENTS),
    };
    let v = u32::try_from(value).map_err(|_| out_of_range())?;
    if (MIN_TRANSACTION_STATEMENTS..=MAX_TRANSACTION_STATEMENTS).contains(&v) {
        Ok(v)
    } else {
        Err(out_of_range())
    }
}

/// トランザクションの持続時間・文数の上限（実装既定値。spec 由来の数値ではない。
/// `docs/design/explicit-transaction.md` 参照）。明示・暗黙（WIRE-16）の両方に効く。
/// 起動時 CLI（Issue #1553）からの設定は `validate_*` を通した値だけを
/// `EngineCore::with_transaction_limits` で注入する。
#[derive(Debug, Clone, Copy)]
pub struct TransactionLimits {
    pub max_duration: Duration,
    pub max_statements: u32,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_millis(DEFAULT_TRANSACTION_DURATION_MS),
            max_statements: DEFAULT_TRANSACTION_STATEMENTS,
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
    /// クライアントへ最後に通知した TimeZone の表示値（wire-server の ParameterStatus 重複抑止の
    /// 記録。TABLE-33・Issue #1575）。`SessionState` ではなくここに置くのは、`ROLLBACK`／abort が
    /// `SessionState` ごと巻き戻す一方、「通知済みの値」は巻き戻してはならない（巻き戻すと
    /// 取り消しで戻った値が通知されなくなる）ため。トランザクション状態機械とは無関係な、
    /// 接続単位の記録を `txn`（全応答経路が既に受け取る接続単位の値）に相乗りさせている。
    reported_time_zone: Option<String>,
}

impl<'e> SessionTransaction<'e> {
    pub fn new(limits: TransactionLimits) -> Self {
        Self {
            state: TxnState::Idle,
            limits,
            generation: 0,
            reported_time_zone: None,
        }
    }

    /// 最後にクライアントへ通知した TimeZone の表示値（未通知なら `None`）。
    pub fn reported_time_zone(&self) -> Option<&str> {
        self.reported_time_zone.as_deref()
    }

    /// `display` を通知済みとして記録する（wire-server が ParameterStatus を書いた直後に呼ぶ）。
    pub fn set_reported_time_zone(&mut self, display: &str) {
        self.reported_time_zone = Some(display.to_string());
    }

    /// 現在の状態（#943 が `ReadyForQuery` の状態バイトへ写像する想定の照会 API）。
    pub fn status(&self) -> TransactionStatus {
        match &self.state {
            TxnState::Idle => TransactionStatus::Idle,
            TxnState::Active(_) => TransactionStatus::InTransaction,
            TxnState::Failed { .. } => TransactionStatus::Failed,
        }
    }

    /// `Active` のときのトランザクション開始時刻（UTC マイクロ秒。`now()` 系の値。
    /// SQL-37・Issue #1586。`core.rs` の `Active` 中の読み取り文が時刻を確定するために呼ぶ）。
    pub(crate) fn txn_start_micros(&self) -> Option<i64> {
        match &self.state {
            TxnState::Active(active) => Some(active.txn_start_micros),
            _ => None,
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

    /// 開始中のトランザクションのアクセスモード（SQL-38・Issue #1932）。`Active` のときだけ
    /// `Some`。#1934 が書き込み文の `25006` 判定に使う。
    pub fn access_mode(&self) -> Option<TxnAccessMode> {
        match &self.state {
            TxnState::Active(active) => Some(active.access_mode),
            _ => None,
        }
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
        self.begin_with_mode(storage, session, TxnAccessMode::ReadWrite)
    }

    /// アクセスモード付きの `BEGIN`（SQL-38・Issue #1932。`core.rs` の実行分岐が呼ぶ）。
    /// `ReadOnly` は #1933／#1934 まで `Idle` 分岐の先頭で `0A000` 拒否する暫定ゲート
    /// （書き込み可能な Tx を読み取り専用として渡さない fail-closed）。
    pub fn begin_with_mode(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
        access_mode: TxnAccessMode,
    ) -> Result<(), SqlSurfaceError> {
        // 暗黙トランザクションはメッセージ内で完結する。明示 `BEGIN` が届くのは
        // 呼び出し側の不変条件違反（制御文を含むメッセージは暗黙にならない）なので、
        // 暗黙の `Active` は abort して `Idle` へ戻し、内部エラーで拒否する。
        if self.abort_if_implicit() {
            return Err(internal_error());
        }
        self.begin_with_origin(storage, session, TxnOrigin::Explicit, access_mode)
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
        self.begin_with_origin(
            storage,
            session,
            TxnOrigin::Implicit,
            TxnAccessMode::ReadWrite,
        )
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
                    writer,
                    temp,
                    started_at,
                    has_writes,
                    session_at_begin,
                    written_by_tenant,
                    written_tables,
                    storage,
                    ..
                } = *active;
                if started_at.elapsed() > self.limits.max_duration {
                    drop(writer);
                    *session = session_at_begin;
                    return Err(SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE));
                }
                commit_active(
                    storage,
                    writer,
                    temp,
                    has_writes,
                    &written_by_tenant,
                    &written_tables,
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
        // `writer` は drop（abort）され、内包する permit はその後に解放される。
        self.state = TxnState::Idle;
        true
    }

    fn begin_with_origin(
        &mut self,
        storage: &'e crate::storage::Storage,
        session: &SessionState,
        origin: TxnOrigin,
        access_mode: TxnAccessMode,
    ) -> Result<(), SqlSurfaceError> {
        match &self.state {
            TxnState::Idle => {
                // 暫定ゲート（#1933／#1934 で除去）: 読み取り専用を宣言した接続へ書き込み可能な
                // Tx を渡さないよう、writer を取る前に拒否する（状態は `Idle` のまま）。
                if access_mode == TxnAccessMode::ReadOnly {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "BEGIN READ ONLY is not supported yet",
                    ));
                }
                // 壁時計の読み取り失敗（範囲外）は writer を開く前に fail-closed で返す
                // （状態は `Idle` のまま）。
                let txn_start_micros = crate::sql::time_fn::now_micros()?;
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
                // 読み取りスナップショットは writer（permit）取得の後に開く。permit 保持中に
                // 開けば文ごとの `begin_read_txn()` が返すものと同じ確定状態になり挙動が
                // 変わらない（writer の前に開くとゲート待ちの間の他者の commit を跨ぎうる）。
                // 失敗時は writer を解放して `Idle` のまま固定文言の `XX000` で拒否する
                // （fail-closed）。
                let read_snapshot = {
                    use redb::ReadableDatabase;
                    match storage.db().begin_read() {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            drop(write_txn);
                            return Err(internal_error());
                        }
                    }
                };
                let next_generation = match self.generation.checked_add(1) {
                    Some(g) => g,
                    None => {
                        // 読み取りスナップショットは関数を抜ける際に併せて解放される。
                        drop(write_txn);
                        return Err(SqlSurfaceError::Internal {
                            detail: "internal error".to_string(),
                        });
                    }
                };
                self.generation = next_generation;
                self.state = TxnState::Active(Box::new(ActiveTxn {
                    origin,
                    access_mode,
                    writer: Some(write_txn),
                    read_snapshot,
                    temp: None,
                    started_at: Instant::now(),
                    txn_start_micros,
                    statements: 0,
                    seen_operation_ids: HashSet::new(),
                    written_tables: HashSet::new(),
                    written_by_tenant: std::collections::BTreeSet::new(),
                    has_writes: false,
                    session_at_begin: session.clone(),
                    cursors: crate::sql::cursor::CursorRegistry::new(),
                    storage,
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
                    writer,
                    temp,
                    started_at,
                    has_writes,
                    session_at_begin,
                    written_by_tenant,
                    written_tables,
                    storage,
                    ..
                } = *active;
                if started_at.elapsed() > self.limits.max_duration {
                    // `writer` を drop（abort）し、内包する permit を解放する。
                    drop(writer);
                    self.state = TxnState::Failed {
                        session_at_begin: Box::new(session_at_begin),
                        expired: false,
                    };
                    return Err(SqlSurfaceError::payload_too_large(LIMIT_EXCEEDED_MESSAGE));
                }
                // 状態は冒頭の `mem::replace` で既に `Idle`。成否によらず `Idle`。
                commit_active(
                    storage,
                    writer,
                    temp,
                    has_writes,
                    &written_by_tenant,
                    &written_tables,
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
                // `writer` は先に drop（abort）してから `permit` を解放する
                // （構造体のフィールド宣言順どおりの drop 順序。writer_gate
                // モジュールドキュメント参照）。
                *session = active.session_at_begin;
                self.state = TxnState::Idle;
                Ok(())
            }
            TxnState::Failed {
                session_at_begin, ..
            } => {
                *session = *session_at_begin;
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

    /// 表を書かない永続状態の書き込み（`setval` の採番状態行。TABLE-24・Issue #1691）を記録する。
    /// `has_writes` だけを立てる（立てないと COMMIT が書き込み txn を破棄して永続化が落ちる）。
    /// `written_tables`・`written_by_tenant` には偽の名前を入れない（遅延 FK 検査の対象は増えない）。
    pub(crate) fn mark_sequence_written(&mut self) {
        if let TxnState::Active(active) = &mut self.state {
            active.has_writes = true;
        }
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

    /// `table` の内容が同一トランザクション内で変わっている（dirty）かどうか
    /// （`sql::relation::ensure_relations_not_written` が使う）。文が直接書き込んだ
    /// テーブルに加え、参照アクションの連鎖で書き換わった子テーブルも含む
    /// （[`Self::dirty_tables`]。連鎖先を取りこぼすと、確定済みスナップショットの
    /// 古い内容を黙って返してしまう）。dirty 集合の取得に失敗した場合は
    /// fail-closed に「dirty」として扱う。
    pub(crate) fn table_already_written(&self, table: &str) -> bool {
        match &self.state {
            TxnState::Active(_) => self
                .dirty_tables()
                .map_or(true, |dirty| dirty.contains(table)),
            _ => false,
        }
    }

    /// このトランザクションで内容が変わりうるテーブル名の集合（Issue #1179）。
    /// 文が直接書き込んだテーブル（`mark_written`）と、テーブル世代が確定済み
    /// スナップショットから変化したテーブル（参照アクションの連鎖で書き込まれた
    /// 子テーブルを含む）の和。読み取りの経路選択（未 commit 変更を読む必要が
    /// あるか）と COMMIT 時の遅延 FK 検査対象の決定に使う。`Active` 以外では空。
    /// 世代の読み取りに失敗した場合は fail-closed に `Err`（呼び出し元は文を
    /// 失敗させる）。書き込みが 1 件も無い間（`has_writes == false`）は世代の
    /// 走査を省き空集合を返す（連鎖書き込みは直接の書き込みが起点のため
    /// 取りこぼさない）。
    pub(crate) fn dirty_tables(&self) -> Result<HashSet<String>, SqlSurfaceError> {
        match &self.state {
            TxnState::Active(active) if !active.has_writes => Ok(HashSet::new()),
            // writer 未取得で書き込み記録がある状態は不変条件違反。`Ok(empty)` にせず `Err` を返し、
            // 呼び出し元（`table_already_written` 等）が fail-closed に dirty とみなす。
            TxnState::Active(active) => match active.writer.as_deref() {
                Some(w) => collect_dirty_tables(active.storage, w, &active.written_tables),
                None => Err(internal_error()),
            },
            _ => Ok(HashSet::new()),
        }
    }

    /// 一時側の書き込みトランザクションが未開始なら開く（`Active` のときのみ。TABLE-44・
    /// Issue #1684）。一時表の最初の文（読み取りを含む。自分の書き込みを読めるようにするため）で
    /// 呼ぶ。一時 DB の単一ライタを既に保持している場合は待たずに `55P03`（再入しない）。
    pub(crate) fn ensure_temp_txn(
        &mut self,
        store: &std::sync::Arc<crate::sql::temp_rows::TempRowStore>,
    ) -> Result<(), SqlSurfaceError> {
        let TxnState::Active(active) = &mut self.state else {
            return Err(internal_error());
        };
        if active.temp.is_none() {
            active.temp = Some(TempTxn {
                txn: store.begin_write()?,
                store: std::sync::Arc::clone(store),
            });
        }
        Ok(())
    }

    /// 開いている一時側の書き込みトランザクション（無ければ `None`）。
    pub(crate) fn temp_write_txn(&self) -> Option<&crate::storage::GatedWriteTxn> {
        match &self.state {
            TxnState::Active(active) => active.temp.as_ref().map(|t| &t.txn),
            _ => None,
        }
    }

    /// 現在保持している共有 `redb::WriteTransaction`（`Active` かつ writer 取得済みの
    /// ときのみ `Some`）。書き込み系の実行本体が [`crate::tenant::WriteTarget::InTxn`]
    /// を構築するために使う。
    pub(crate) fn write_txn(&self) -> Option<&redb::WriteTransaction> {
        match &self.state {
            TxnState::Active(active) => active.writer.as_deref(),
            _ => None,
        }
    }

    /// `BEGIN` 時点の確定済み読み取りスナップショット（`Active` のときのみ `Some`。
    /// Issue #2399）。本番経路からはまだ呼ばれず、#2400・#2403 が読み取り経路へ結線する。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn read_snapshot(&self) -> Option<&redb::ReadTransaction> {
        match &self.state {
            TxnState::Active(active) => Some(&active.read_snapshot),
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
                session_at_begin: Box::new(active.session_at_begin),
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
                session_at_begin: Box::new(active.session_at_begin),
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

/// [`SessionTransaction::dirty_tables`] の実体。`written` に、書き込みトランザクション
/// 内の世代が `storage` の確定済み世代から変化したテーブルを加えて返す。
fn collect_dirty_tables(
    storage: &crate::storage::Storage,
    write_txn: &redb::WriteTransaction,
    written: &HashSet<String>,
) -> Result<HashSet<String>, SqlSurfaceError> {
    let internal = || SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    };
    use redb::ReadableDatabase;
    let committed = storage.db().begin_read().map_err(|_| internal())?;
    let changed = crate::catalog::changed_table_generations_in_write_txn(write_txn, &committed)
        .map_err(|_| internal())?;
    let mut dirty = written.clone();
    dirty.extend(changed);
    Ok(dirty)
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
/// 扱いで `XX000`。成功時は `ON COMMIT` を適用する。呼び出し元は事前に `state` を `Idle` にすること。
// 引数は `ActiveTxn` を分解して渡す 2 つの呼び出し元（明示・暗黙の commit）専用で、構造体に
// まとめ直しても取り出し・再構築が増えるだけのため、個別の引数のままにする。
#[allow(clippy::too_many_arguments)]
fn commit_active(
    storage: &crate::storage::Storage,
    writer: Option<crate::storage::GatedWriteTxn>,
    temp: Option<TempTxn>,
    has_writes: bool,
    written_by_tenant: &std::collections::BTreeSet<(String, String)>,
    written_tables: &HashSet<String>,
    session_at_begin: SessionState,
    session: &mut SessionState,
) -> Result<(), SqlSurfaceError> {
    // writer 未取得で書き込み記録がある状態は不変条件違反（未 commit 変更の取りこぼしを
    // 黙認しない）。fail-closed に開始時点へ復元して `XX000`。
    if has_writes && writer.is_none() {
        *session = session_at_begin;
        return Err(internal_error());
    }
    if let (true, Some(write_txn)) = (has_writes, writer.as_deref()) {
        // 遅延 FK 検査の対象は「書き込んだテナント × dirty テーブル」の直積
        // （Issue #1179）。`mark_written` が記録するのは文が直接対象にしたテーブル
        // だけで、参照アクション（`CASCADE`・`SET NULL`・`SET DEFAULT`）の連鎖で
        // 書き換わった子テーブルは含まれない。連鎖先は別の `INITIALLY DEFERRED` FK
        // を持ちうるため、記録だけに頼ると COMMIT で違反を見逃す（fail-open）。
        // テーブル世代（書き換えた全経路が bump する契約）由来の dirty 集合との和を
        // 取り、テナント集合は上位集合（fail-closed）として全書き込みテナントを使う。
        let dirty = match collect_dirty_tables(storage, write_txn, written_tables) {
            Ok(dirty) => dirty,
            Err(_) => {
                drop(writer);
                *session = session_at_begin;
                return Err(internal_error());
            }
        };
        let tenants: std::collections::BTreeSet<&String> =
            written_by_tenant.iter().map(|(tenant, _)| tenant).collect();
        let pairs = tenants
            .iter()
            .flat_map(|tenant| dirty.iter().map(move |table| (*tenant, table)));
        for (tenant_id, table) in pairs {
            if let Err(e) =
                crate::constraint::enforce_deferred_foreign_keys_in_txn(write_txn, tenant_id, table)
            {
                // `writer` を drop（abort）し permit を解放したうえで開始時点の
                // セッション状態を復元する（commit 自体の失敗と同じロールバック扱い）。
                drop(writer);
                *session = session_at_begin;
                return Err(crate::sql::exec::map_write_error(e, "commit"));
            }
        }
    }
    // 一時側の commit・`ON COMMIT` は書き込みの有無によらず実行するため、writer が無くても
    // ここで早期 return しない。
    let result = match writer {
        Some(w) if has_writes => crate::recovery::commit_boundary::commit(w),
        other => {
            drop(other);
            Ok(())
        }
    };
    result.map_err(|_| {
        // commit 失敗はロールバック扱い（writer は消費済みで、permit も解放されている）。
        *session = session_at_begin;
        internal_error()
    })?;
    // 永続側の commit 成功後にだけ、一時側を確定し、続けて一時表の `ON COMMIT` 動作を適用する
    // （TABLE-44・Issue #1683・#1684。ROLLBACK・永続側の commit 失敗では `temp` を drop（abort）
    // するだけで呼ばない）。順序は「永続 commit → 一時 commit → ON COMMIT」で、一時 DB の単一ライタを
    // `ON COMMIT` が取り直せるよう、一時側を先に確定して書き込みトランザクションを手放す。
    // 永続側は既に commit 済みのため、一時側の commit・`ON COMMIT` が失敗すると、クライアントには
    // 失敗（`XX000`）が返るが永続側は commit 済み、という食い違いが起こりうる（2 つの DB の commit は
    // 原子的でない。一時表は in-memory redb で発生確率は極めて低い既知の制約。docs/design/temp-table.md）。
    if let Some(temp) = temp {
        temp.commit()?;
    }
    session.apply_temp_on_commit()
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

    #[test]
    fn limits_default_matches_constants() {
        let d = TransactionLimits::default();
        assert_eq!(d.max_duration, Duration::from_secs(20));
        assert_eq!(d.max_statements, 1_000);
    }

    #[test]
    fn validate_duration_boundaries() {
        assert_eq!(
            validate_max_transaction_duration_ms(100),
            Ok(Duration::from_millis(100))
        );
        assert_eq!(
            validate_max_transaction_duration_ms(60_000),
            Ok(Duration::from_secs(60))
        );
        for bad in [0, 99, 60_001, u64::MAX] {
            assert!(validate_max_transaction_duration_ms(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validate_statements_boundaries() {
        assert_eq!(validate_max_transaction_statements(1), Ok(1));
        assert_eq!(validate_max_transaction_statements(10_000), Ok(10_000));
        for bad in [0, 10_001, u64::from(u32::MAX) + 1, u64::MAX] {
            assert!(validate_max_transaction_statements(bad).is_err(), "{bad}");
        }
        let e = validate_max_transaction_statements(0).expect_err("zero");
        assert_eq!(
            e.to_string(),
            "max transaction statements value 0 is out of range 1..=10000"
        );
    }
    use super::*;

    fn tokens_of(sql: &str) -> Vec<Token> {
        crate::sql::lexer::tokenize(sql).expect("tokenize")
    }

    fn validated(sql: &str) -> Result<ValidatedTxnControl, SqlSurfaceError> {
        validate_transaction_control_tokens(&tokens_of(sql))
    }

    #[test]
    fn begin_variants_are_accepted_without_access_mode() {
        for sql in ["BEGIN", "BEGIN WORK", "BEGIN TRANSACTION", "begin"] {
            let v = validated(sql).expect("accepted");
            assert_eq!(v.control, TxnControl::Begin);
            assert_eq!(v.access_mode, None);
        }
    }

    #[test]
    fn begin_access_mode_is_accepted() {
        for (sql, mode) in [
            ("BEGIN READ ONLY", TxnAccessMode::ReadOnly),
            ("BEGIN READ WRITE", TxnAccessMode::ReadWrite),
            ("BEGIN WORK READ ONLY", TxnAccessMode::ReadOnly),
            ("BEGIN TRANSACTION READ WRITE", TxnAccessMode::ReadWrite),
            ("begin read only", TxnAccessMode::ReadOnly),
        ] {
            let v = validated(sql).expect("accepted");
            assert_eq!(v.control, TxnControl::Begin, "{sql}");
            assert_eq!(v.access_mode, Some(mode), "{sql}");
        }
    }

    #[test]
    fn commit_and_rollback_variants_are_accepted() {
        for (sql, ctrl) in [
            ("COMMIT", TxnControl::Commit),
            ("COMMIT WORK", TxnControl::Commit),
            ("ROLLBACK", TxnControl::Rollback),
            ("ROLLBACK TRANSACTION", TxnControl::Rollback),
        ] {
            let v = validated(sql).unwrap();
            assert_eq!(v.control, ctrl);
            assert_eq!(v.access_mode, None);
        }
    }

    #[test]
    fn trailing_clauses_are_rejected() {
        for sql in [
            "BEGIN READ",
            "BEGIN ONLY",
            "BEGIN WRITE",
            "BEGIN READ ONLY READ WRITE",
            "BEGIN READ ONLY, READ ONLY",
            "BEGIN READ ONLY,",
            "BEGIN , READ ONLY",
            "BEGIN READ ONLY DEFERRABLE",
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "BEGIN READ ONLY ISOLATION LEVEL SERIALIZABLE",
            "COMMIT READ ONLY",
            "ROLLBACK READ WRITE",
            "ROLLBACK TO SAVEPOINT x",
            "START",
            "START WORK",
            "START TRANSACTION READ ONLY",
            "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "END foo",
            "ABORT TO SAVEPOINT x",
            "BEGIN AND CHAIN",
        ] {
            let err = validated(sql).unwrap_err();
            assert_eq!(err.wire_code(), "42601", "{sql}");
        }
    }

    /// SQL-69 (1)・Issue #1930: 別名が対応する制御種別へ写像される。
    #[test]
    fn aliases_are_accepted() {
        for (sql, want) in [
            ("START TRANSACTION", TxnControl::StartTransaction),
            ("start transaction", TxnControl::StartTransaction),
            ("END", TxnControl::Commit),
            ("END WORK", TxnControl::Commit),
            ("END TRANSACTION", TxnControl::Commit),
            ("ABORT", TxnControl::Rollback),
            ("ABORT WORK", TxnControl::Rollback),
            ("abort transaction", TxnControl::Rollback),
        ] {
            let ctrl = validated(sql).unwrap().control;
            assert_eq!(ctrl, want, "{sql}");
            assert!(is_transaction_control_head(&tokens_of(sql)), "{sql}");
        }
        assert!(!is_transaction_control_head(&tokens_of("START foo")));
    }

    /// Issue #1930: 未対応の `AND [NO] CHAIN`・`PREPARE TRANSACTION` は `0A000`。
    #[test]
    fn chain_and_prepare_are_feature_not_supported() {
        for sql in [
            "COMMIT AND CHAIN",
            "ROLLBACK AND CHAIN",
            "END AND CHAIN",
            "ABORT AND NO CHAIN",
            "COMMIT WORK AND CHAIN",
            "PREPARE TRANSACTION 'x'",
        ] {
            let err = validate_transaction_control_tokens(&tokens_of(sql)).unwrap_err();
            assert_eq!(err.wire_code(), "0A000", "{sql}");
        }
    }

    #[test]
    fn rollback_statement_detection_includes_abort() {
        assert!(is_rollback_statement("ABORT"));
        assert!(is_rollback_statement("abort work"));
        assert!(is_rollback_statement("ROLLBACK"));
        assert!(!is_rollback_statement("END"));
        assert!(!is_rollback_statement("COMMIT"));
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

    /// 一時 DB に表を 1 つ作った `Storage` を返すテスト補助（Issue #2399 の単体テスト用）。
    fn open_test_storage(
        tag: &str,
    ) -> (
        crate::storage::Storage,
        crate::test_util::temp_db::CleanupGuard,
    ) {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
        let path = unique_db_path(tag);
        let guard = CleanupGuard(path.clone());
        let storage = crate::storage::Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "documents",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        (storage, guard)
    }

    /// `BEGIN` 後は writer と読み取りスナップショットの両方を保持し、終了経路
    /// （COMMIT・ROLLBACK・fail・暗黙の abort）の後はどちらも手放す
    /// （SQL-31・PERSIST-4・TASK-333、Issue #2399）。
    #[test]
    fn begin_holds_writer_and_read_snapshot() {
        let (storage, _g) = open_test_storage("txn-2399-holds");
        let mut session = SessionState::default();
        let mut txn = SessionTransaction::new(TransactionLimits::default());

        txn.begin(&storage, &session).expect("begin");
        assert!(txn.write_txn().is_some());
        assert!(txn.read_snapshot().is_some());
        txn.commit(&mut session).expect("commit");
        assert!(txn.write_txn().is_none() && txn.read_snapshot().is_none());

        txn.begin(&storage, &session).expect("begin");
        txn.rollback(&mut session).expect("rollback");
        assert!(txn.write_txn().is_none() && txn.read_snapshot().is_none());

        txn.begin(&storage, &session).expect("begin");
        txn.fail();
        assert_eq!(txn.status(), TransactionStatus::Failed);
        assert!(txn.write_txn().is_none() && txn.read_snapshot().is_none());
        txn.rollback(&mut session).expect("rollback");

        txn.begin_implicit(&storage, &session)
            .expect("begin_implicit");
        assert!(txn.write_txn().is_some() && txn.read_snapshot().is_some());
        txn.abort_implicit();
        assert!(txn.write_txn().is_none() && txn.read_snapshot().is_none());
    }

    /// `BEGIN` 直後のスナップショットは、新たに開いた確定済み読み取りと同じ世代を指す
    /// （writer 取得後に開くため挙動が変わらない、という不変条件。Issue #2399）。
    #[test]
    fn snapshot_matches_committed_state_at_begin() {
        use redb::ReadableDatabase;
        let (storage, _g) = open_test_storage("txn-2399-gen");
        let session = SessionState::default();
        let mut txn = SessionTransaction::new(TransactionLimits::default());
        txn.begin(&storage, &session).expect("begin");
        let read_gen = |r: &redb::ReadTransaction| {
            r.open_table(crate::storage::GENERATION_TABLE)
                .expect("open generation table")
                .get("generation")
                .expect("get")
                .map(|v| v.value())
        };
        let fresh = storage.db().begin_read().expect("begin_read");
        let snapshot = txn.read_snapshot().expect("snapshot");
        assert_eq!(read_gen(snapshot), read_gen(&fresh));
    }

    /// writer 未取得のまま書き込み記録がある状態は不変条件違反で、COMMIT・dirty 判定とも
    /// fail-closed に倒れ、`session` は `BEGIN` 時点へ復元される（Issue #2399）。
    #[test]
    fn commit_without_writer_but_with_writes_is_fail_closed() {
        use crate::sql::mode::SearchMode;
        let (storage, _g) = open_test_storage("txn-2399-failclosed");
        let mut session = SessionState::default();
        let mut txn = SessionTransaction::new(TransactionLimits::default());
        txn.begin(&storage, &session).expect("begin");
        session.set_search_mode(SearchMode::Recall);
        if let TxnState::Active(active) = &mut txn.state {
            drop(active.writer.take());
            active.has_writes = true;
        }
        assert!(txn.dirty_tables().is_err());
        assert!(txn.table_already_written("documents"));
        let err = txn.commit(&mut session).expect_err("fail-closed");
        assert!(matches!(err, SqlSurfaceError::Internal { .. }));
        assert_eq!(txn.status(), TransactionStatus::Idle);
        assert_eq!(session.search_mode(), None);
    }

    /// writer 未取得で書き込みも無ければ COMMIT は成功して `Idle` へ戻る
    /// （後続 #2400 の「読み取りだけのトランザクション」の形。Issue #2399）。
    #[test]
    fn commit_without_writer_and_without_writes_is_ok() {
        let (storage, _g) = open_test_storage("txn-2399-readonly");
        let mut session = SessionState::default();
        let mut txn = SessionTransaction::new(TransactionLimits::default());
        txn.begin(&storage, &session).expect("begin");
        if let TxnState::Active(active) = &mut txn.state {
            drop(active.writer.take());
        }
        txn.commit(&mut session).expect("commit");
        assert_eq!(txn.status(), TransactionStatus::Idle);
    }

    /// `redb::ReadTransaction` を持たせても `SessionTransaction` が `Send` のままであること
    /// （wire-server が接続スレッドで保持するため。Issue #2399）。
    #[test]
    fn session_transaction_stays_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SessionTransaction<'static>>();
    }
}
