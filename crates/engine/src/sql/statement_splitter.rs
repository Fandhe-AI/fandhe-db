//! 簡易クエリプロトコル 1 メッセージに含まれるセミコロン区切りの複数 SQL 文を
//! 分割する（WIRE-16・TASK-219。ポインタ: `docs/spec/04-behavior/wire-protocol.md`
//! WIRE-16・`docs/spec/05-tasks.md` TASK-219）。
//!
//! 呼び出し文脈: `wire-server::simple_query::execute_and_respond`（`'Q'` 本文の
//! オーケストレーション）が本モジュールを呼び、`Statements(..)` を得た場合は
//! 各要素を [`crate::core::EngineCore::execute_sql_in_session`] へ 1 文ずつ渡す。
//! wire 層は SQL の字句・構文知識を持たず、分割自体をここへ委譲する
//! （モジュール境界: `.claude/rules/coding-rust.md`）。
//!
//! # 分割規則
//!
//! 通常状態の `;` だけを区切りとする字句走査で分割する。走査は次の 5 状態を持つ:
//! 通常／`'...'`（`''` エスケープ）／`"..."`（`""` エスケープ）／`--` から改行
//! （`\n` または `\r`）まで／`/* ... */`（PostgreSQL と同じく入れ子を数える）。
//! コメント・引用の内部にある `;` は区切りとみなさない。各断片は加工せず（コメントも
//! 除去せず）そのまま engine へ渡すため、コメントや二重引用符識別子を含む断片は
//! 単一文のときと同じく lexer が `42601` で拒否する（WIRE-16 の「文ごとに独立して
//! 許可リストを検証し、複数文であることを理由に受理範囲を変えない」に従う）。
//!
//! **PostgreSQL と分割結果が食い違いうる構文では分割しない**（fail-closed）。
//! 未終端の `'...'`・`"..."`・`/* */`、通常状態の `$`（ドル引用）、E 文字列の接頭辞
//! （`E'`／`e'`）を検出した場合は [`SplitOutcome::Single`] を返し、全文を engine へ
//! そのまま渡す（lexer が `42601` で拒否し、1 文も実行されない）。これにより
//! 「区切りの解釈差を突いて後続の文を密輸する」経路を構造的に塞ぐ。
//!
//! # 原子性（暗黙トランザクション）の扱い
//!
//! [`plan_multi_statement`] が、メッセージの実行方式を選ぶ。
//!
//! - セッションが既にトランザクション中（`Active`／`Failed`）、またはメッセージが
//!   `BEGIN`／`COMMIT`／`ROLLBACK` を含む場合は [`MultiStatementPlan::Sequential`]
//!   （既存の [`check_write_placement`] を適用し、トランザクション外の書き込みは
//!   最後の 1 文に限る）。
//! - 書き込みが最後の 1 文だけ、または書き込みが無い場合も `Sequential`
//!   （文ごとの autocommit。従来と同一）。
//! - それ以外（書き込みが最後以外の位置にある）は
//!   [`MultiStatementPlan::ImplicitTransaction`]。メッセージ全体を 1 つの暗黙
//!   トランザクション（`sql::transaction::SessionTransaction::begin_implicit`）で
//!   実行し、途中でエラーになれば先行する書き込みも残さない（WIRE-16・SQL-31・
//!   RECOVER-12）。暗黙トランザクション内で使える文は明示トランザクションと同じ
//!   許可リストであり、対応外の文は `0A000` でトランザクション全体をロールバックする
//!   （`docs/design/wire-multi-statement.md` 参照）。

use crate::error_format::{ClassifiedError, ErrorClass};
use crate::sql::lexer::{tokenize, Keyword, Token};

/// 1 メッセージで許容する非空文の上限（実装既定値。WIRE-16 のポインタ）。
/// 超過は [`MultiStatementError::TooManyStatements`]（`54000`）で fail-closed に
/// 拒否し、untrusted 入力に対する無制限 `Vec` 確保を避ける
/// （`.claude/rules/coding-rust.md`）。
pub const MAX_STATEMENTS_PER_QUERY: usize = 16;

/// [`split_statements`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome<'a> {
    /// 非空文が 0 個または 1 個で、既存 engine（`sql::allowlist::
    /// expect_end_of_statement`）がそのまま受理する形（`;` なし、または末尾に
    /// 1 個だけの `;`）。呼び出し元は元テキストを無加工でそのまま渡す。単一文の
    /// 既存挙動（応答バイト列・エラーコード・メッセージ）を構造的に不変に保つ。
    Single,
    /// 非空文が 0 個（`;` のみ・空白のみ・`;;` のみ等）。呼び出し元は
    /// EmptyQueryResponse を返す。
    Empty,
    /// 非空文が 2 個以上、または `Single` の条件を満たさない 1 個（先頭に `;` が
    /// ある等）。各要素は通常状態の `;`（区切り）を含まず前後の空白を trim 済み。
    Statements(Vec<&'a str>),
}

/// 複数文実行の分割・配置検証エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiStatementError {
    /// 非空文の数が [`MAX_STATEMENTS_PER_QUERY`] を超過した（`54000`）。
    TooManyStatements,
    /// 書き込み系文（[`StatementEffect::Write`]）が最後の文以外の位置にある
    /// （`0A000`）。[`check_write_placement`] が返す。制御文を含まないメッセージでは
    /// [`plan_multi_statement`] が暗黙トランザクションへ振り分けるため、
    /// 呼び出し元へ届くのは制御文（`BEGIN`／`COMMIT`／`ROLLBACK`）を含む場合のみ。
    /// 本モジュールのドキュメント「原子性」節参照。
    WriteNotLast,
}

impl ClassifiedError for MultiStatementError {
    fn error_class(&self) -> ErrorClass {
        match self {
            MultiStatementError::TooManyStatements => ErrorClass::PayloadTooLarge,
            MultiStatementError::WriteNotLast => ErrorClass::FeatureNotSupported,
        }
    }

    fn client_message(&self) -> String {
        match self {
            MultiStatementError::TooManyStatements => {
                format!("too many statements in one query message (max {MAX_STATEMENTS_PER_QUERY})")
            }
            MultiStatementError::WriteNotLast => {
                "write statements (INSERT/UPDATE/DELETE/TRUNCATE) outside a transaction \
                 are only supported as the last statement in a multi-statement query \
                 that contains transaction control statements"
                    .to_string()
            }
        }
    }
}

/// 1 文が持つ副作用の分類。[`check_write_placement`] が「書き込みは最後だけ」の
/// 制約判定に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementEffect {
    /// 検索 SELECT・`EXPLAIN`（読み取り専用。commit を伴わない）。
    ReadOnly,
    /// `SET ...`・`CREATE FUNCTION ...`（接続セッション内で完結し、redb commit を
    /// 伴わない）。
    SessionLocal,
    /// `INSERT`（UPSERT 含む）・`UPDATE`・`DELETE`・`TRUNCATE`、および読み取り専用・
    /// セッション局所のいずれとも判定できない未知の先頭語（fail-closed の既定。
    /// 将来 engine に書き込み系構文が追加された場合に誤って許可しないための
    /// 安全側既定）。
    Write,
    /// 字句解析に失敗する、または字句解析には成功しても構造上どの
    /// `core.rs::execute_sql_in_session` の分岐にも到達し得ない先頭トークン形
    /// （`Token::Number`・`Token::Punct` 等）。実行すれば必ず `validate_sql` の
    /// 許可リスト外（`42601`）で拒否され副作用が起きないため、位置に関わらず
    /// 許可する（`check_write_placement` の対象外）。
    Rejected,
    /// `BEGIN`／`COMMIT`／`ROLLBACK`（SQL-31・TASK-221）。[`check_write_placement`]
    /// がトランザクション状態を模擬する際の遷移点になる。
    TransactionControl(crate::sql::transaction::TxnControl),
}

/// SQL テキストをセミコロン区切りの文へ分割する。コメント・引用（`'...'`・
/// `"..."`）の内部にある `;` では分割せず、PostgreSQL と分割結果が食い違いうる
/// 構文（未終端の引用・コメント、`$`、E 文字列）を検出した場合は分割せず
/// [`SplitOutcome::Single`] を返す（モジュールドキュメント参照）。
///
/// untrusted 入力経路のため `get()` と `saturating_*` のみを使い、
/// `unwrap`/`expect`/添字アクセスは使わない（`.claude/rules/coding-rust.md`）。
/// 区切り・引用符・コメント記号はすべて ASCII のため、バイト単位の走査でも UTF-8 の
/// 文字境界を壊さない（`str::get` が境界でなければ `None` を返す防御も併用する）。
pub fn split_statements(input: &str) -> Result<SplitOutcome<'_>, MultiStatementError> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    // 1 パスで非空文を直接切り出す（`;` の位置をいったん `Vec<usize>` へ
    // 集めてから 2 パス目で切り出す方式は、`;;;;...` のような入力で
    // 「区切りだけの無制限 `Vec` 確保」に相当し untrusted 入力経路の防御的
    // 上限（`.claude/rules/coding-rust.md`「無制限リソース確保」対応）と
    // 相性が悪いため避ける。`statements` は上限到達時点で即座に `Err` を
    // 返すため `MAX_STATEMENTS_PER_QUERY + 1` 要素までしか伸びない）。
    let mut statements: Vec<&str> = Vec::new();
    let mut semicolon_count: usize = 0;
    let mut start = 0usize;

    while let Some(&b) = bytes.get(i) {
        let next = bytes.get(i.saturating_add(1)).copied();
        match b {
            b'\'' | b'"' => match skip_quoted(bytes, i, b) {
                Some(end) => i = end,
                // 未終端の引用。lexer が同一入力を必ず拒否するため分割せず全文を渡す。
                None => return Ok(SplitOutcome::Single),
            },
            b'-' if next == Some(b'-') => {
                i = skip_line_comment(bytes, i);
            }
            b'/' if next == Some(b'*') => match skip_block_comment(bytes, i) {
                Some(end) => i = end,
                None => return Ok(SplitOutcome::Single),
            },
            // ドル引用（`$tag$...$tag$`）は PostgreSQL と分割結果が食い違いうる
            // うえ、`tokenize` も `$` を拒否する。分割せず全文を渡す。
            b'$' => return Ok(SplitOutcome::Single),
            // E 文字列（`E'...'`）はバックスラッシュエスケープの解釈が異なり、
            // 引用の終端位置が PostgreSQL と食い違いうる。識別子の途中の `e`
            // （`name'..'` 等）は接頭辞ではないので除外する。
            b'E' | b'e' if next == Some(b'\'') && !is_ident_continue_before(bytes, i) => {
                return Ok(SplitOutcome::Single);
            }
            b';' => {
                let piece = input.get(start..i).unwrap_or("").trim();
                if !piece.is_empty() {
                    statements.push(piece);
                    if statements.len() > MAX_STATEMENTS_PER_QUERY {
                        return Err(MultiStatementError::TooManyStatements);
                    }
                }
                semicolon_count = semicolon_count.saturating_add(1);
                // `;` は ASCII 1 バイトなので `i + 1` は必ず次の文字境界。
                i = i.saturating_add(1);
                start = i;
            }
            _ => i = i.saturating_add(1),
        }
    }

    let tail = input.get(start..).unwrap_or("");
    let tail_trimmed = tail.trim();
    if !tail_trimmed.is_empty() {
        statements.push(tail_trimmed);
        if statements.len() > MAX_STATEMENTS_PER_QUERY {
            return Err(MultiStatementError::TooManyStatements);
        }
    }

    match statements.len() {
        0 => Ok(SplitOutcome::Empty),
        1 => match semicolon_count {
            0 => Ok(SplitOutcome::Single),
            // 唯一の `;` が文の直後（後ろは空白のみ）であれば既存の単一文
            // 経路（`expect_end_of_statement` がそのまま受理する形）と同じ
            // 意味になるため `Single` を返す。先頭に `;` がある場合
            // （`;SELECT 1` 等）は `tail_trimmed`（＝唯一の `;` の後ろの
            // テキスト）が非空になるためここに該当せず `Statements` へ回す
            // （元テキストのままでは先頭の `;` トークンで構文エラーになる
            // ため、除去した形で渡す必要がある）。
            1 if tail_trimmed.is_empty() => Ok(SplitOutcome::Single),
            _ => Ok(SplitOutcome::Statements(statements)),
        },
        _ => Ok(SplitOutcome::Statements(statements)),
    }
}

/// `bytes[open]` が開き引用符 `quote`（`'` または `"`）であるときに、閉じ引用符の
/// 直後の位置を返す（連続 2 個の引用符はエスケープとして継続する。
/// `lexer::lex_string_literal` と同じ規則）。未終端なら `None`。
fn skip_quoted(bytes: &[u8], open: usize, quote: u8) -> Option<usize> {
    let mut j = open.saturating_add(1);
    loop {
        let c = *bytes.get(j)?;
        if c == quote {
            if bytes.get(j.saturating_add(1)) == Some(&quote) {
                j = j.saturating_add(2);
                continue;
            }
            return Some(j.saturating_add(1));
        }
        j = j.saturating_add(1);
    }
}

/// `--` で始まる行コメントの終端（`\n`／`\r` の位置。無ければ入力末尾）を返す。
fn skip_line_comment(bytes: &[u8], open: usize) -> usize {
    let mut j = open.saturating_add(2);
    while let Some(&c) = bytes.get(j) {
        if c == b'\n' || c == b'\r' {
            break;
        }
        j = j.saturating_add(1);
    }
    j
}

/// `/*` で始まるブロックコメントの終端の直後の位置を返す。PostgreSQL と同じく
/// 入れ子を数える（数えないと `/* a /* b */ ; INSERT ... */` の `INSERT` が分割
/// されて実行されてしまう）。深さは `usize` の飽和演算で数え、追加の確保はしない。
/// 未終端なら `None`。
fn skip_block_comment(bytes: &[u8], open: usize) -> Option<usize> {
    let mut j = open.saturating_add(2);
    let mut depth: usize = 1;
    loop {
        let c = *bytes.get(j)?;
        let n = bytes.get(j.saturating_add(1)).copied();
        if c == b'/' && n == Some(b'*') {
            depth = depth.saturating_add(1);
            j = j.saturating_add(2);
        } else if c == b'*' && n == Some(b'/') {
            depth = depth.saturating_sub(1);
            j = j.saturating_add(2);
            if depth == 0 {
                return Some(j);
            }
        } else {
            j = j.saturating_add(1);
        }
    }
}

/// `bytes[at]` の直前のバイトが識別子の構成文字（英数字・`_`・非 ASCII）か。
fn is_ident_continue_before(bytes: &[u8], at: usize) -> bool {
    match at.checked_sub(1).and_then(|p| bytes.get(p)) {
        Some(&c) => c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80,
        None => false,
    }
}
/// 1 文の先頭トークンから [`StatementEffect`] を判定する。判定語彙は
/// `core.rs::execute_sql_in_session` の先頭トークン覗き見分岐（`INSERT`／
/// `TRUNCATE`／`DELETE`／`UPDATE`／`DROP`）と `sql::allowlist::validate_sql` が
/// 受理する `SET`／`CREATE FUNCTION`／`SELECT`／`EXPLAIN` に対応づける。
/// `DROP`（`DROP TABLE`。SQL-23・TASK-203、Issue #902）は個別の判定分岐を
/// 持たず、`Token::Ident(_) => StatementEffect::Write` の fail-closed 既定
/// （未知の先頭語は書き込み扱い）へ自然に落ちる——`DROP` は [`Keyword`] へ
/// 含まれないため常に `Token::Ident` として字句解析される
/// （`lexer::keyword_from_str` 参照）。
pub fn classify_statement(stmt: &str) -> StatementEffect {
    let tokens = match tokenize(stmt) {
        Ok(t) => t,
        Err(_) => return StatementEffect::Rejected,
    };
    let Some(first) = tokens.first() else {
        return StatementEffect::Rejected;
    };

    match first {
        Token::Keyword(Keyword::Select) => StatementEffect::ReadOnly,
        Token::Ident(name) if name.eq_ignore_ascii_case("EXPLAIN") => StatementEffect::ReadOnly,
        // WIRE-15・TASK-218: `DECLARE`／`FETCH`／`CLOSE`（カーソル）はいずれも
        // redb の commit を伴わない（`DECLARE` は既存の読み取り経路を 1 回
        // 実行するのみ・`FETCH`／`CLOSE` はセッション内メモリ状態のみを操作する）
        // ため `ReadOnly` に分類する。誤って `Write` 扱いにすると、複数文
        // メッセージ内でトランザクション外の `DECLARE` 等が最後の文以外に
        // あるだけで `0A000`（`WriteNotLast`）になり、本来の `25P01`／`34000`
        // より先に紛らわしいエラーへ倒れてしまう。
        Token::Ident(name) if name.eq_ignore_ascii_case("DECLARE") => StatementEffect::ReadOnly,
        Token::Ident(name) if name.eq_ignore_ascii_case("FETCH") => StatementEffect::ReadOnly,
        Token::Ident(name) if name.eq_ignore_ascii_case("CLOSE") => StatementEffect::ReadOnly,
        // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）は
        // `sql::allowlist::validate_sql_tokens` の `WITH` 分岐が主クエリを
        // 広域取得（`Statement::Scan`）のみに限定し、データ変更 CTE
        // （`WITH x AS (DELETE ...)` 等）は本文が `SELECT` 以外のため構造検証
        // 段で `42601` になり `Statement` 自体が生成されない。したがって
        // `WITH` を `ReadOnly` に分類しても、複数文メッセージの書き込み配置
        // 制約（本関数の呼び出し元）をすり抜けて副作用が生じることはない。
        Token::Ident(name) if name.eq_ignore_ascii_case("WITH") => StatementEffect::ReadOnly,
        Token::Ident(name) if name.eq_ignore_ascii_case("SET") => StatementEffect::SessionLocal,
        Token::Ident(name) if name.eq_ignore_ascii_case("BEGIN") => {
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Begin)
        }
        Token::Ident(name) if name.eq_ignore_ascii_case("COMMIT") => {
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Commit)
        }
        Token::Ident(name) if name.eq_ignore_ascii_case("ROLLBACK") => {
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Rollback)
        }
        Token::Ident(name) if name.eq_ignore_ascii_case("CREATE") => match tokens.get(1) {
            Some(Token::Ident(next)) if next.eq_ignore_ascii_case("FUNCTION") => {
                StatementEffect::SessionLocal
            }
            // `CREATE` の直後が `FUNCTION` でない未知の形（将来 `CREATE TABLE` 等が
            // 追加された場合を含む）は fail-closed に書き込み扱いとする。
            _ => StatementEffect::Write,
        },
        // Issue #1175: `COPY ... TO STDOUT` は読み取り専用（commit を伴わない）、
        // `COPY ... FROM STDIN` は書き込み。カタログを引けないため、括弧の深さ 0 で
        // 最初に現れる `FROM`／`TO` で判定し、どちらも見つからなければ書き込み扱い
        // （fail-closed）にする。
        Token::Ident(name) if name.eq_ignore_ascii_case("COPY") => classify_copy(&tokens),
        Token::Ident(_) => StatementEffect::Write,
        // `Select` 以外の `Keyword`（`From`/`Where`/`And`/`Order`/`By`/`Limit`）・
        // `Number`・`Punct`・`StringLiteral`・比較/距離演算子・`QualifiedIdent` は
        // いずれも `Token::Ident` を要求する書き込み分岐に到達できない先頭トークン
        // 形であり、必ず `validate_sql` の許可リスト外（`42601`）へ落ちる。
        _ => StatementEffect::Rejected,
    }
}

/// `COPY` 文のトークン列を [`StatementEffect`] へ分類する（[`classify_statement`] の
/// 補助）。括弧の深さ 0 の位置で先頭の `COPY` の後に最初に現れる `TO` なら
/// `ReadOnly`、それ以外（`FROM`・どちらも無い）は `Write`。
/// `COPY (SELECT ... FROM t) TO STDOUT` の内側の `FROM` は深さ 1 なので無視する。
fn classify_copy(tokens: &[Token]) -> StatementEffect {
    let mut depth: usize = 0;
    for token in tokens.iter().skip(1) {
        match token {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Keyword(Keyword::From) if depth == 0 => return StatementEffect::Write,
            Token::Ident(word) if depth == 0 && word.eq_ignore_ascii_case("TO") => {
                return StatementEffect::ReadOnly
            }
            _ => {}
        }
    }
    StatementEffect::Write
}

/// 複数文メッセージの実行方式（[`plan_multi_statement`] の結果。Issue #1175・
/// WIRE-16）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiStatementPlan {
    /// 文ごとに順次実行する（既存方式。トランザクション外の書き込みは最後の 1 文のみ）。
    Sequential,
    /// メッセージ全体を 1 つの暗黙トランザクションで実行し、最後の文の成功後に
    /// 1 回だけ commit する。途中のエラーは全体のロールバックになる。
    ImplicitTransaction,
}

/// 複数文メッセージの実行方式を決める。`session_in_txn` は本メッセージの先頭文
/// 実行前にセッションが `Idle` でない（`Active` または `Failed`）ことを表す。
///
/// 既存経路を暗黙トランザクションで包み直さない: `Active` では許可リストに載った
/// 文しか実行できず、今動いているメッセージ（例: `SELECT ...; UPDATE ...`）が
/// `0A000` へ後退してしまうため、[`check_write_placement`] が受理する形は
/// 必ず `Sequential` に残す。`ImplicitTransaction` になるのは、制御文を含まず、
/// トランザクション外で書き込みが最後以外にある場合だけ。未知の先頭語は
/// [`StatementEffect::Write`] なので、この分岐へ振り分けられたうえで `Active` の
/// 許可リストにより拒否される（autocommit で素通りする経路は生まれない）。
pub fn plan_multi_statement(
    stmts: &[&str],
    session_in_txn: bool,
) -> Result<MultiStatementPlan, MultiStatementError> {
    let has_control = stmts.iter().any(|s| {
        matches!(
            classify_statement(s),
            StatementEffect::TransactionControl(_)
        )
    });
    if session_in_txn || has_control {
        check_write_placement(stmts, session_in_txn)?;
        return Ok(MultiStatementPlan::Sequential);
    }
    match check_write_placement(stmts, false) {
        Ok(()) => Ok(MultiStatementPlan::Sequential),
        Err(MultiStatementError::WriteNotLast) => Ok(MultiStatementPlan::ImplicitTransaction),
        Err(other) => Err(other),
    }
}

/// 書き込み系文（[`StatementEffect::Write`]）が最後の文以外にある場合を拒否する
/// （モジュールドキュメント「原子性」節参照）。`initially_in_txn` は本メッセージの
/// 先頭文実行前のセッションが既に明示トランザクション中（`Active`）かどうかを表す
/// （SQL-31・TASK-221。`BEGIN` でトランザクション内、`COMMIT`／`ROLLBACK` で
/// トランザクション外という遷移を先頭から模擬し、`BEGIN` を含むメッセージ内では
/// 位置に関わらず書き込みを許可する。`COMMIT` は必ず最後の文でのみ許可し、
/// 「1 メッセージにつき commit は高々 1 回」という既存の不変条件を維持する）。
pub fn check_write_placement(
    stmts: &[&str],
    initially_in_txn: bool,
) -> Result<(), MultiStatementError> {
    use crate::sql::transaction::TxnControl;
    let last_index = stmts.len().saturating_sub(1);
    let mut in_txn = initially_in_txn;
    for (i, stmt) in stmts.iter().enumerate() {
        match classify_statement(stmt) {
            StatementEffect::Write if !in_txn && i != last_index => {
                return Err(MultiStatementError::WriteNotLast);
            }
            StatementEffect::TransactionControl(TxnControl::Begin) => {
                in_txn = true;
            }
            StatementEffect::TransactionControl(TxnControl::Commit) => {
                if i != last_index {
                    return Err(MultiStatementError::WriteNotLast);
                }
                in_txn = false;
            }
            StatementEffect::TransactionControl(TxnControl::Rollback) => {
                in_txn = false;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(outcome: SplitOutcome<'_>) -> Vec<&str> {
        match outcome {
            SplitOutcome::Statements(v) => v,
            other => panic!("expected Statements, got {other:?}"),
        }
    }

    #[test]
    fn literal_semicolon_is_not_a_split_point() {
        let outcome = split_statements("SELECT 1 WHERE lang = 'a;b'").expect("split");
        assert_eq!(outcome, SplitOutcome::Single);
    }

    #[test]
    fn escaped_quote_next_to_semicolon_does_not_break_literal_tracking() {
        // `'a''; b'` は 1 個の文字列リテラル（内容 `a'; b`）。エスケープ直後の
        // `;` はリテラル内なので分割点にならない。
        let outcome = split_statements("SELECT 'a''; b'").expect("split");
        assert_eq!(outcome, SplitOutcome::Single);
    }

    #[test]
    fn multiple_statements_with_literal_semicolons_split_correctly() {
        let outcome = split_statements("SELECT 1 WHERE lang = 'a;b'; SELECT 2").expect("split");
        assert_eq!(
            statements(outcome),
            vec!["SELECT 1 WHERE lang = 'a;b'", "SELECT 2"]
        );
    }

    #[test]
    fn double_semicolon_and_leading_trailing_semicolons_are_ignored() {
        assert_eq!(split_statements(";;").expect("split"), SplitOutcome::Empty);
        assert_eq!(
            split_statements(" ; ; ").expect("split"),
            SplitOutcome::Empty
        );
        assert_eq!(split_statements(";").expect("split"), SplitOutcome::Empty);
    }

    #[test]
    fn plain_single_statement_variants_are_single() {
        assert_eq!(
            split_statements("SELECT 1").expect("split"),
            SplitOutcome::Single
        );
        assert_eq!(
            split_statements("SELECT 1;").expect("split"),
            SplitOutcome::Single
        );
        assert_eq!(
            split_statements("SELECT 1;  ").expect("split"),
            SplitOutcome::Single
        );
    }

    #[test]
    fn single_non_empty_statement_with_extra_semicolons_is_statements() {
        assert_eq!(
            statements(split_statements("SELECT 1;;").expect("split")),
            vec!["SELECT 1"]
        );
        assert_eq!(
            statements(split_statements(";SELECT 1").expect("split")),
            vec!["SELECT 1"]
        );
        assert_eq!(
            statements(split_statements("SELECT 1; ;").expect("split")),
            vec!["SELECT 1"]
        );
    }

    #[test]
    fn sixteen_statements_are_accepted_seventeen_are_rejected() {
        let sixteen = "SELECT 1;".repeat(16);
        let outcome = split_statements(&sixteen).expect("split");
        assert_eq!(statements(outcome).len(), 16);

        let seventeen = "SELECT 1;".repeat(17);
        assert_eq!(
            split_statements(&seventeen),
            Err(MultiStatementError::TooManyStatements)
        );
    }

    #[test]
    fn empty_statements_do_not_count_toward_the_limit() {
        // 16 個の非空文の間に空文（`;;`）を挟んでも上限には数えない。
        let input = "SELECT 1;;".repeat(16);
        let outcome = split_statements(&input).expect("split");
        assert_eq!(statements(outcome).len(), 16);
    }

    #[test]
    fn comments_and_double_quotes_are_split_lexically_and_each_piece_is_rejected_by_lexer() {
        // コメント・二重引用符内の `;` は区切りにならず、外側の `;` では分割される。
        // 断片は加工されないため、lexer は各断片を単一文のときと同じく拒否する。
        let cases: [(&str, Vec<&str>); 5] = [
            (
                "SELECT 1 -- comment; SELECT 2\n; SELECT 3",
                vec!["SELECT 1 -- comment; SELECT 2", "SELECT 3"],
            ),
            (
                "SELECT 1 /* comment; */; SELECT 2",
                vec!["SELECT 1 /* comment; */", "SELECT 2"],
            ),
            (
                "SELECT \"a;b\" FROM t; SELECT 2",
                vec!["SELECT \"a;b\" FROM t", "SELECT 2"],
            ),
            (
                "SELECT \"a\"\"; b\" FROM t; SELECT 2",
                vec!["SELECT \"a\"\"; b\" FROM t", "SELECT 2"],
            ),
            ("SELECT 1; -- done", vec!["SELECT 1", "-- done"]),
        ];
        for (input, expected) in cases {
            let pieces = statements(split_statements(input).expect("split"));
            assert_eq!(pieces, expected, "input: {input}");
            let rejected = pieces.iter().filter(|p| tokenize(p).is_err()).count();
            assert!(rejected >= 1, "lexer must reject a piece of: {input}");
        }
    }

    #[test]
    fn unterminated_constructs_and_ambiguous_syntax_fall_back_to_single() {
        let inputs = [
            "SELECT 'unterminated; SELECT 2",
            "SELECT \"unterminated; SELECT 2",
            "SELECT 1 /* unterminated; SELECT 2",
            "SELECT 1; /* a /* b */ ; SELECT 2",
            "SELECT $1; SELECT 2",
            "SELECT $$a;b$$; SELECT 2",
            "SELECT E'a\\';b'; SELECT 2",
        ];
        for input in inputs {
            assert_eq!(
                split_statements(input).expect("split"),
                SplitOutcome::Single,
                "input: {input}"
            );
            assert!(tokenize(input).is_err(), "lexer must also reject: {input}");
        }
    }

    #[test]
    fn nested_block_comments_do_not_smuggle_a_statement() {
        // 入れ子を数えないと内側の `*/` でコメントが閉じ、`; INSERT ...` が
        // 独立した文として切り出されてしまう。
        let input = "SELECT 1; /* a /* b */ ; INSERT INTO t VALUES (1) */ ; SELECT 2";
        let pieces = statements(split_statements(input).expect("split"));
        assert_eq!(
            pieces,
            vec![
                "SELECT 1",
                "/* a /* b */ ; INSERT INTO t VALUES (1) */",
                "SELECT 2"
            ]
        );
        assert!(tokenize(pieces[1]).is_err());
    }

    #[test]
    fn line_comment_ends_at_carriage_return_or_line_feed() {
        let pieces = statements(split_statements("SELECT 1 -- c\r; SELECT 2").expect("split"));
        assert_eq!(pieces, vec!["SELECT 1 -- c", "SELECT 2"]);
        // `;` が行コメント内にあれば区切りにならない（改行まで続く）。
        assert_eq!(
            split_statements("SELECT 1 -- c; SELECT 2").expect("split"),
            SplitOutcome::Single
        );
    }

    #[test]
    fn e_in_identifier_is_not_an_e_string_prefix() {
        let pieces = statements(
            split_statements("SELECT 1 WHERE name = 'x'; SELECT 2 WHERE role = 'y'")
                .expect("split"),
        );
        assert_eq!(pieces.len(), 2);
    }

    #[test]
    fn plan_multi_statement_selects_sequential_or_implicit_transaction() {
        let ins = "INSERT INTO t (id) VALUES (1) USING OPERATION_ID 'o1'";
        let ins2 = "INSERT INTO t (id) VALUES (2) USING OPERATION_ID 'o2'";
        // 書き込みが最後だけ・書き込みなしは従来どおり Sequential。
        assert_eq!(
            plan_multi_statement(&["SELECT 1", ins], false),
            Ok(MultiStatementPlan::Sequential)
        );
        assert_eq!(
            plan_multi_statement(&["SELECT 1", "SELECT 2"], false),
            Ok(MultiStatementPlan::Sequential)
        );
        // 書き込みが最後以外なら暗黙トランザクション。
        assert_eq!(
            plan_multi_statement(&[ins, ins2], false),
            Ok(MultiStatementPlan::ImplicitTransaction)
        );
        assert_eq!(
            plan_multi_statement(&[ins, "SELECT 1"], false),
            Ok(MultiStatementPlan::ImplicitTransaction)
        );
        // 未知の先頭語は Write 扱いなので暗黙トランザクションへ振り分けられる。
        assert_eq!(
            plan_multi_statement(&["VACUUM t", "SELECT 1"], false),
            Ok(MultiStatementPlan::ImplicitTransaction)
        );
        // セッションがトランザクション中、または制御文を含むなら既存の配置検査。
        assert_eq!(
            plan_multi_statement(&[ins, ins2], true),
            Ok(MultiStatementPlan::Sequential)
        );
        assert_eq!(
            plan_multi_statement(&["BEGIN", ins, ins2, "COMMIT"], false),
            Ok(MultiStatementPlan::Sequential)
        );
        assert_eq!(
            plan_multi_statement(&[ins, "BEGIN", ins2], false),
            Err(MultiStatementError::WriteNotLast)
        );
        assert_eq!(
            plan_multi_statement(&["BEGIN", "COMMIT", "SELECT 1"], false),
            Err(MultiStatementError::WriteNotLast)
        );
    }

    #[test]
    fn classify_copy_distinguishes_to_and_from() {
        assert_eq!(
            classify_statement("COPY (SELECT id FROM t) TO STDOUT"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("copy (select 1) to stdout with (format csv)"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("COPY t (id) FROM STDIN USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(classify_statement("COPY t"), StatementEffect::Write);
    }

    #[test]
    fn multibyte_characters_in_literals_do_not_break_offsets() {
        let outcome = split_statements("SELECT 1 WHERE body = 'あ;い'; SELECT 2").expect("split");
        assert_eq!(
            statements(outcome),
            vec!["SELECT 1 WHERE body = 'あ;い'", "SELECT 2"]
        );
    }

    #[test]
    fn classify_statement_covers_all_effects() {
        assert_eq!(classify_statement("SELECT 1"), StatementEffect::ReadOnly);
        assert_eq!(
            classify_statement("EXPLAIN SELECT 1"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("explain select 1"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("SET search_mode = 'recall'"),
            StatementEffect::SessionLocal
        );
        assert_eq!(
            classify_statement("CREATE FUNCTION f(x) AS x"),
            StatementEffect::SessionLocal
        );
        assert_eq!(
            classify_statement("CREATE TABLE t (id INT)"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("INSERT INTO t (id) VALUES (1) USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("UPDATE t SET body = 'x' WHERE id = 1 USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("DELETE FROM t WHERE id = 1 USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("TRUNCATE TABLE t USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        // TASK-202・SQL-23（Issue #900）: `ALTER` は `lexer::Keyword` へ含めない
        // ため `Token::Ident(_) => StatementEffect::Write` の既存分岐がそのまま
        // 適用される（`sql::allowlist::validate_alter_table` と同一情報源）。
        assert_eq!(
            classify_statement("ALTER TABLE t ADD COLUMN note TEXT"),
            StatementEffect::Write
        );
        // Issue #902（SQL-23・TASK-203）: `DROP TABLE` は書き込み系（DDL）の
        // ため、複文メッセージ中で最後以外に置かれた場合は他の書き込み文と
        // 同じく `0A000` で拒否されなければならない（`check_write_placement`
        // ドキュメント参照）。
        assert_eq!(classify_statement("DROP TABLE t"), StatementEffect::Write);
        // TASK-206・INDEX-7（Issue #908）: 索引宣言 DDL も書き込み系（カタログを
        // commit する）として分類され、複文中で最後以外なら `0A000`。
        assert_eq!(
            classify_statement("CREATE INDEX i ON t (body)"),
            StatementEffect::Write
        );
        assert_eq!(classify_statement("DROP INDEX i"), StatementEffect::Write);
        // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）は読み取り系
        // （`sql::allowlist::validate_sql_tokens` の `WITH` 分岐が受理する形は
        // 主クエリが広域取得のみで、書き込みを持たない）。
        assert_eq!(
            classify_statement("WITH x AS (SELECT id FROM t) SELECT * FROM x LIMIT 1"),
            StatementEffect::ReadOnly
        );
        assert!(check_write_placement(&["CREATE INDEX i ON t (body)", "SELECT 1"], false).is_err());
        assert!(check_write_placement(&["SELECT 1", "DROP INDEX i"], false).is_ok());
        assert_eq!(
            classify_statement("BEGIN"),
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Begin)
        );
        assert_eq!(
            classify_statement("COMMIT"),
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Commit)
        );
        assert_eq!(
            classify_statement("ROLLBACK"),
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Rollback)
        );
        assert_eq!(
            classify_statement("SELECT 'unterminated"),
            StatementEffect::Rejected
        );
        assert_eq!(classify_statement("123"), StatementEffect::Rejected);
        assert_eq!(classify_statement("(SELECT 1)"), StatementEffect::Rejected);
        assert_eq!(classify_statement("FROM t"), StatementEffect::Rejected);
    }

    #[test]
    fn check_write_placement_allows_write_only_as_last_statement() {
        assert!(check_write_placement(&["SELECT 1", "INSERT INTO t VALUES (1)"], false).is_ok());
        assert!(check_write_placement(&["INSERT INTO t VALUES (1)", "SELECT 1"], false).is_err());
        assert!(check_write_placement(
            &["INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (2)"],
            false
        )
        .is_err());
        assert!(check_write_placement(
            &["SELECT 'unterminated", "INSERT INTO t VALUES (1)"],
            false
        )
        .is_ok());
        assert!(check_write_placement(&["INSERT INTO t VALUES (1)"], false).is_ok());
        assert!(check_write_placement(&[], false).is_ok());
    }

    #[test]
    fn check_write_placement_allows_writes_anywhere_inside_a_begin_block() {
        assert!(check_write_placement(
            &[
                "BEGIN",
                "INSERT INTO t VALUES (1)",
                "INSERT INTO t VALUES (2)",
                "COMMIT",
            ],
            false
        )
        .is_ok());
        // `COMMIT` は必ず最後の文でのみ許可する。
        assert!(check_write_placement(&["BEGIN", "COMMIT", "SELECT 1"], false).is_err());
        // `initially_in_txn = true`（すでに `Active`）なら先頭の書き込みも許可する。
        assert!(check_write_placement(&["INSERT INTO t VALUES (1)", "SELECT 1"], true).is_ok());
    }

    /// Issue #902（SQL-23・TASK-203）: `DROP TABLE x; SELECT ...` のような
    /// メッセージが、`DROP` を書き込み分類の対象外として扱う抜け穴により
    /// fail-open で通過しないことを固定する（`classify_statement` ドキュメント
    /// 参照）。
    #[test]
    fn check_write_placement_rejects_drop_table_not_last() {
        assert!(check_write_placement(&["DROP TABLE docs", "SELECT 1"], false).is_err());
        assert!(check_write_placement(&["SELECT 1", "DROP TABLE docs"], false).is_ok());
    }
}
