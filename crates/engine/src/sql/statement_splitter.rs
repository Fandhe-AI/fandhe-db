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
//! コメント・引用の内部にある `;` は区切りとみなさない。コメントと `"..."` の終端走査は
//! lexer（`sql::lexer::line_comment_end` 等）と同じ関数を共有し、両者の解釈が食い違わ
//! ないようにしている。各断片は加工せず（コメントも除去せず）そのまま engine へ渡し、
//! lexer がコメントを読み飛ばす。コメントと空白だけの断片は空文として無視する
//! （Issue #1346）。`"..."` の中身が許可形（英数字・`_`・予約語でない）でなければ、
//! その文だけが lexer の `42601` になる（WIRE-16 の「文ごとに独立して許可リストを
//! 検証し、複数文であることを理由に受理範囲を変えない」に従う）。
//!
//! SQL 標準形の関数本体（`CREATE FUNCTION ... BEGIN ATOMIC <文>; ... END`。SQL-67 (2)・Issue #2219）は、
//! 通常状態の語を追跡して本体の内側の `;` で分割しない。文の先頭語が `CREATE` で `BEGIN` の直後が
//! `ATOMIC` のときだけ入れ子の深さを数え始め（`CASE` で +1、`END` で -1。識別子にもなる `BEGIN` は数えない）、深さが 0 に戻るまで
//! `;` を区切りとしない。入力の終端までに閉じない場合・深さが上限を超える場合は `Single`（fail-closed。
//! engine が `42601` で拒否し、1 文も実行しない）。
//!
//! **PostgreSQL と分割結果が食い違いうる構文では分割しない**（fail-closed）。
//! 未終端の `'...'`・`"..."`・`/* */`、通常状態の `$<数字>`・未終端／識別子隣接のドル引用、E 文字列の接頭辞
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
use crate::sql::lexer::{
    block_comment_end, dollar_quote_scan, line_comment_end, quoted_identifier_end, tokenize,
    DollarQuoteScan, Keyword, Token,
};

/// 1 メッセージで許容する非空文の上限（実装既定値。WIRE-16 のポインタ）。
/// 超過は [`MultiStatementError::TooManyStatements`]（`54000`）で fail-closed に
/// 拒否し、untrusted 入力に対する無制限 `Vec` 確保を避ける
/// （`.claude/rules/coding-rust.md`）。
pub const MAX_STATEMENTS_PER_QUERY: usize = 16;

/// `BEGIN ATOMIC` 本体の入れ子（`CASE`）の深さの上限（Issue #2219）。超過は `Single`。
const MAX_ATOMIC_DEPTH: usize = 64;

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
    /// 文が 2 つ以上のメッセージに分割実行 DML（`UPDATE`／`DELETE ... PARTITIONED`）または
    /// `CANCEL PARTITIONED DML` が含まれる（`25001`。Issue #1129）。複数文メッセージは暗黙
    /// トランザクションに相当し、分割実行はトランザクション内で実行できないため、黙って
    /// 原子実行や autocommit へ切り替えず全体を拒否する（何も実行しない）。
    PartitionedInMultiStatement,
}

impl ClassifiedError for MultiStatementError {
    fn error_class(&self) -> ErrorClass {
        match self {
            MultiStatementError::TooManyStatements => ErrorClass::PayloadTooLarge,
            MultiStatementError::WriteNotLast => ErrorClass::FeatureNotSupported,
            MultiStatementError::PartitionedInMultiStatement => ErrorClass::ActiveSqlTransaction,
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
            // 単一文での `PartitionedDmlInTransaction` と同じ固定文言。
            MultiStatementError::PartitionedInMultiStatement => {
                "partitioned DML cannot run inside a transaction block".to_string()
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
    // 現在の断片に、空白・コメント以外の文字が 1 つでもあるか。
    let mut has_content = false;
    // `BEGIN ATOMIC` 追跡（Issue #2219）: 現在の文の先頭語が `CREATE` か・何語目か・直前の語が
    // `BEGIN` か・本体の入れ子の深さ（0 は本体の外）。
    let mut stmt_create = false;
    let mut word_count = 0usize;
    let mut prev_begin = false;
    let mut atomic_depth = 0usize;

    while let Some(&b) = bytes.get(i) {
        let next = bytes.get(i.saturating_add(1)).copied();
        match b {
            b'\'' => match skip_quoted(bytes, i, b) {
                Some(end) => {
                    has_content = true;
                    prev_begin = false;
                    i = end;
                }
                // 未終端の引用。lexer が同一入力を必ず拒否するため分割せず全文を渡す。
                None => return Ok(SplitOutcome::Single),
            },
            b'"' => match quoted_identifier_end(input, i) {
                Some(end) => {
                    has_content = true;
                    prev_begin = false;
                    i = end;
                }
                None => return Ok(SplitOutcome::Single),
            },
            b'-' if next == Some(b'-') => {
                i = line_comment_end(input, i);
            }
            b'/' if next == Some(b'*') => match block_comment_end(input, i) {
                Some(end) => i = end,
                None => return Ok(SplitOutcome::Single),
            },
            // ドル引用（`$tag$...$tag$`）は lexer と同じ `dollar_quote_scan` で本文ごと
            // 読み飛ばす。未終端・識別子に隣接する形は lexer も拒否するため分割せず
            // 全文を渡す。`$<数字>` は引用ではないので従来どおり `Single`。
            b'$' => match dollar_quote_scan(input, i) {
                DollarQuoteScan::Quoted { end, .. } => {
                    i = end;
                    has_content = true;
                }
                _ => return Ok(SplitOutcome::Single),
            },
            // E 文字列（`E'...'`）はバックスラッシュエスケープの解釈が異なり、
            // 引用の終端位置が PostgreSQL と食い違いうる。識別子の途中の `e`
            // （`name'..'` 等）は接頭辞ではないので除外する。
            b'E' | b'e' if next == Some(b'\'') && !is_ident_continue_before(bytes, i) => {
                return Ok(SplitOutcome::Single);
            }
            // `BEGIN ATOMIC` 本体の内側の `;` は文の区切りではない。
            b';' if atomic_depth > 0 => {
                has_content = true;
                prev_begin = false;
                i = i.saturating_add(1);
            }
            b';' => {
                stmt_create = false;
                word_count = 0;
                prev_begin = false;
                let piece = input.get(start..i).unwrap_or("").trim();
                if has_content && !piece.is_empty() {
                    statements.push(piece);
                    if statements.len() > MAX_STATEMENTS_PER_QUERY {
                        return Err(MultiStatementError::TooManyStatements);
                    }
                }
                semicolon_count = semicolon_count.saturating_add(1);
                // `;` は ASCII 1 バイトなので `i + 1` は必ず次の文字境界。
                i = i.saturating_add(1);
                start = i;
                has_content = false;
            }
            // 語（識別子・キーワード）。`BEGIN ATOMIC` の追跡に使う（Issue #2219）。
            _ if (b.is_ascii_alphabetic() || b == b'_') && !is_ident_continue_before(bytes, i) => {
                has_content = true;
                let mut j = i.saturating_add(1);
                while bytes
                    .get(j)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
                {
                    j = j.saturating_add(1);
                }
                let word = input.get(i..j).unwrap_or("");
                if word_count == 0 {
                    stmt_create = word.eq_ignore_ascii_case("CREATE");
                }
                word_count = word_count.saturating_add(1);
                if atomic_depth > 0 {
                    // 本体の入れ子は `CASE ... END` だけを数える（本体解析器 `parse_standard_function_body`
                    // と同じ構造判定。`BEGIN` は非予約語で引数名・列名にもなり、標準形の本体は入れ子の
                    // `BEGIN` 文を受理しないため深さに含めない）。
                    if word.eq_ignore_ascii_case("CASE") {
                        atomic_depth = atomic_depth.saturating_add(1);
                        if atomic_depth > MAX_ATOMIC_DEPTH {
                            return Ok(SplitOutcome::Single);
                        }
                    } else if word.eq_ignore_ascii_case("END") {
                        atomic_depth = atomic_depth.saturating_sub(1);
                    }
                } else if stmt_create && prev_begin && word.eq_ignore_ascii_case("ATOMIC") {
                    atomic_depth = 1;
                }
                prev_begin = stmt_create && atomic_depth == 0 && word.eq_ignore_ascii_case("BEGIN");
                i = j;
            }
            _ if b.is_ascii() => {
                if !b.is_ascii_whitespace() {
                    has_content = true;
                    prev_begin = false;
                }
                i = i.saturating_add(1);
            }
            // 非 ASCII は 1 文字単位で読み、Unicode 空白（`trim`・lexer と同じ基準）
            // だけなら内容なしとして扱う。
            _ => {
                let ch = input.get(i..).and_then(|r| r.chars().next());
                match ch {
                    Some(c) => {
                        if !c.is_whitespace() {
                            has_content = true;
                        }
                        i = i.saturating_add(c.len_utf8());
                    }
                    None => {
                        has_content = true;
                        i = i.saturating_add(1);
                    }
                }
            }
        }
    }

    // `BEGIN ATOMIC` の本体が入力の終端までに閉じない。分割せず全文を渡して拒否させる。
    if atomic_depth > 0 {
        return Ok(SplitOutcome::Single);
    }
    let tail = input.get(start..).unwrap_or("");
    let tail_trimmed = tail.trim();
    let tail_has_content = has_content && !tail_trimmed.is_empty();
    if tail_has_content {
        statements.push(tail_trimmed);
        if statements.len() > MAX_STATEMENTS_PER_QUERY {
            return Err(MultiStatementError::TooManyStatements);
        }
    }

    match statements.len() {
        0 => Ok(SplitOutcome::Empty),
        1 => match semicolon_count {
            0 => Ok(SplitOutcome::Single),
            // 唯一の `;` が文の直後（後ろは空白・コメントのみ）であれば既存の単一文
            // 経路（lexer がコメントを読み飛ばし、`expect_end_of_statement` が
            // そのまま受理する形）と同じ意味になるため `Single` を返す。先頭に `;`
            // がある場合（`;SELECT 1` 等）は唯一の `;` の後ろに内容があり
            // `tail_has_content` が真になるためここに該当せず `Statements` へ回す
            // （元テキストのままでは先頭の `;` トークンで構文エラーになる
            // ため、除去した形で渡す必要がある）。
            1 if !tail_has_content => Ok(SplitOutcome::Single),
            _ => Ok(SplitOutcome::Statements(statements)),
        },
        _ => Ok(SplitOutcome::Statements(statements)),
    }
}

/// `bytes[open]` が開き引用符 `quote`（`'`）であるときに、閉じ引用符の
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
        // SQL-67 (5)・Issue #2218: ユーザー定義関数（組み込みでない名前の呼び出し）を含む `SELECT` は、
        // `LANGUAGE sql|plpgsql` の本体が書き込みうるため `Write` に倒す（fail-closed。複数文メッセージの
        // 書き込み配置制約をすり抜けない）。誤って `Write` にしても暗黙トランザクションになるだけ。
        Token::Keyword(Keyword::Select) => {
            // SQL-38・Issue #2230: 行ロック句つき `SELECT` は writer を取得するため `Write` に倒す
            // （fail-closed。複数文メッセージでは暗黙トランザクションとなり、ロックが後続文まで保持される）。
            if calls_non_builtin_function(&tokens) || has_row_lock_clause(&tokens) {
                StatementEffect::Write
            } else {
                StatementEffect::ReadOnly
            }
        }
        Token::Ident(name) if name.eq_ignore_ascii_case("EXPLAIN") => StatementEffect::ReadOnly,
        // Issue #1129: `SHOW PARTITIONED DML` はジョブ表の読み取りのみ（commit を伴わない）。
        // `CANCEL`（書き込み）は未知の先頭語として `Write`（fail-closed）に落とす。
        Token::Ident(name) if name.eq_ignore_ascii_case("SHOW") => StatementEffect::ReadOnly,
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
        // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）は読み取り系。ただし
        // データ変更 CTE（SQL-63・Issue #1875。`(` の直後に `INSERT`／`UPDATE`／`DELETE` が現れる
        // 本文）は書き込みを持ちうる形のため `Write` に倒す（fail-closed）。現時点では
        // `sql::allowlist::validate_sql_tokens` が固定 `0A000` で止めるが、実行結線（#1876）後も
        // 複数文メッセージの書き込み配置制約をすり抜けないよう、分類は先に書き込み側へ置く。
        Token::Ident(name) if name.eq_ignore_ascii_case("WITH") => {
            // CTE 本文の `(` は直前が `AS` か `MATERIALIZED`（`AS [NOT] MATERIALIZED (`）の
            // ものに限る。列名リスト `x(delete)` や式の括弧は本文ではない。
            let has_dml_body = tokens.windows(3).any(|w| {
                matches!(
                    w.first(),
                    Some(Token::Ident(a))
                        if a.eq_ignore_ascii_case("AS") || a.eq_ignore_ascii_case("MATERIALIZED")
                ) && matches!(w.get(1), Some(Token::Punct('(')))
                    && matches!(
                        w.get(2),
                        Some(Token::Ident(n))
                            if n.eq_ignore_ascii_case("INSERT")
                                || n.eq_ignore_ascii_case("UPDATE")
                                || n.eq_ignore_ascii_case("DELETE")
                    )
            });
            if has_dml_body || calls_non_builtin_function(&tokens) || has_row_lock_clause(&tokens) {
                StatementEffect::Write
            } else {
                StatementEffect::ReadOnly
            }
        }
        Token::Ident(name) if name.eq_ignore_ascii_case("SET") => StatementEffect::SessionLocal,
        Token::Ident(name) if name.eq_ignore_ascii_case("BEGIN") => {
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Begin)
        }
        // Issue #1930: `START TRANSACTION` は `TRANSACTION` を伴うときだけ制御文。それ以外の
        // `START ...` は未知の先頭語として下の既定（`Write`。fail-closed）へ倒す。
        Token::Ident(name)
            if name.eq_ignore_ascii_case("START")
                && matches!(tokens.get(1), Some(Token::Ident(n)) if n.eq_ignore_ascii_case("TRANSACTION")) =>
        {
            StatementEffect::TransactionControl(
                crate::sql::transaction::TxnControl::StartTransaction,
            )
        }
        Token::Ident(name)
            if name.eq_ignore_ascii_case("COMMIT") || name.eq_ignore_ascii_case("END") =>
        {
            StatementEffect::TransactionControl(crate::sql::transaction::TxnControl::Commit)
        }
        Token::Ident(name)
            if name.eq_ignore_ascii_case("ROLLBACK") || name.eq_ignore_ascii_case("ABORT") =>
        {
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

/// `calls_non_builtin_function` 用: `名前(` が構文語としての用法と確認できるか。確認できない
/// （呼び出しとも読める）場合は `false`＝呼び出し扱い（`Write`＝fail-closed）。
/// 構文語は予約されないため同名のユーザー関数が定義でき、構文位置でない `(` 直前の出現は呼び出しになる。
///   - 中置語（`IN`・`ON`・`OR`・`IS`・`LIKE`・`BETWEEN`・`AS`・`USING`・`THEN`・`ELSE`・集合演算・
///     `OFFSET`・`GROUP`・`HAVING`・`WITH`・`COLLATE`）: 直前が式の終端のときだけ構文
///     （`NOT IN (` 等は `NOT` の更に前が式の終端のとき）
///   - `NOT`: 述語の文脈語（`WHERE`・`AND`・`OR`・`HAVING`・`ON`・`WHEN`・`THEN`・`ELSE`）の直後だけ構文
///   - `EXISTS`・`ARRAY`: 直後が副問い合わせ（`SELECT`／`WITH`／`VALUES`）のときだけ構文
///   - `WHEN`: 直前が `CASE` か式の終端のときだけ構文
///   - `EXTRACT`・`OVERLAY`: 引数内に `FROM`・`PLACING` の構文形があるときだけ構文
///   - 窓関数名: 対応する `)` の直後が `OVER` のときだけ構文
///   - `VALUES`・`DISTINCT`・`ROW`・`UNNEST`: 式の先頭の呼び出しと区別できないため常に呼び出し扱い
///   - `FROM`・`BY`: `Ident` で届くのは引用識別子のみ（構文の `FROM`・`BY` は `Keyword`）なので常に呼び出し扱い
///   - 上記以外（`JSON_EXISTS` 等の専用構文）は従来どおり構文語
fn syntax_form_confirmed(
    tokens: &[Token],
    i: usize,
    name: &str,
    expr_prefix_words: &[&str],
) -> bool {
    let is = |w: &str| name.eq_ignore_ascii_case(w);
    let ident_at = |idx: usize, words: &[&str]| {
        matches!(
            tokens.get(idx),
            Some(Token::Ident(a)) if words.iter().any(|w| a.eq_ignore_ascii_case(w))
        )
    };
    // 位置 `idx` の直前が式の終端（識別子・`)`・リテラル。式演算子語・句の語は除く）か。
    let terminator_before = |idx: usize| {
        idx > 0
            && match tokens.get(idx - 1) {
                Some(Token::Punct(')'))
                | Some(Token::QualifiedIdent { .. })
                | Some(Token::Number(_))
                | Some(Token::StringLiteral(_)) => true,
                Some(Token::Ident(a)) => {
                    !expr_prefix_words.iter().any(|w| a.eq_ignore_ascii_case(w))
                }
                _ => false,
            }
    };
    let subquery_follows = matches!(
        tokens.get(i.saturating_add(2)),
        Some(Token::Keyword(Keyword::Select))
    ) || ident_at(i.saturating_add(2), &["WITH", "VALUES"]);
    // `(` に対応する `)` の位置（なければ `None`）。
    let matching_close = || {
        let mut depth = 0usize;
        for (j, tk) in tokens.iter().enumerate().skip(i.saturating_add(1)) {
            match tk {
                Token::Punct('(') => depth = depth.saturating_add(1),
                Token::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    };
    // `FROM`・`BY` は引用符なしならトークナイザが `Keyword` にするため、`Ident` で届くのは引用識別子
    // （`"from"(...)`）だけで、構文ではなく関数呼び出しになる。常に呼び出し扱い（`Write`）とする。
    if is("VALUES") || is("DISTINCT") || is("ROW") || is("UNNEST") || is("FROM") || is("BY") {
        return false;
    }
    if is("NOT") {
        return i > 0
            && (matches!(
                tokens.get(i - 1),
                Some(Token::Keyword(Keyword::Where)) | Some(Token::Keyword(Keyword::And))
            ) || ident_at(i - 1, &["OR", "HAVING", "ON", "WHEN", "THEN", "ELSE"]));
    }
    if is("EXISTS") || is("ARRAY") {
        return subquery_follows;
    }
    if is("WHEN") {
        return (i > 0 && ident_at(i - 1, &["CASE"])) || terminator_before(i);
    }
    if is("EXTRACT") || is("OVERLAY") {
        let target = if is("EXTRACT") { "FROM" } else { "PLACING" };
        let Some(close) = matching_close() else {
            return false;
        };
        let mut depth = 0usize;
        return tokens
            .iter()
            .enumerate()
            .skip(i.saturating_add(2))
            .take_while(|(j, _)| *j < close)
            .any(|(_, tk)| match tk {
                Token::Punct('(') => {
                    depth = depth.saturating_add(1);
                    false
                }
                Token::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    false
                }
                Token::Ident(a) => depth == 0 && a.eq_ignore_ascii_case(target),
                Token::Keyword(Keyword::From) => depth == 0 && target == "FROM",
                _ => false,
            });
    }
    if [
        "ROW_NUMBER",
        "RANK",
        "DENSE_RANK",
        "PERCENT_RANK",
        "CUME_DIST",
        "NTILE",
        "LAG",
        "LEAD",
        "FIRST_VALUE",
        "LAST_VALUE",
        "NTH_VALUE",
    ]
    .iter()
    .any(|w| is(w))
    {
        return matching_close().is_some_and(|close| ident_at(close.saturating_add(1), &["OVER"]));
    }
    if [
        "IN",
        "ON",
        "OR",
        "IS",
        "LIKE",
        "ILIKE",
        "BETWEEN",
        "AS",
        "USING",
        "THEN",
        "ELSE",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "OFFSET",
        "GROUP",
        "HAVING",
        "WITH",
        "COLLATE",
    ]
    .iter()
    .any(|w| is(w))
    {
        if terminator_before(i) {
            return true;
        }
        // `a NOT IN (` / `a NOT LIKE (` / `a IS NOT (` の `NOT` を挟む形。
        return i > 0
            && ident_at(i - 1, &["NOT"])
            && (is("IN") || is("LIKE") || is("ILIKE") || is("BETWEEN") || is("IS"))
            && terminator_before(i - 1);
    }
    true
}

/// 行ロック句（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`）らしき並びを含むか。
/// 括弧の深さは見ない保守的な検出（誤検出は `Write` 側へ倒れるだけ）。
fn has_row_lock_clause(tokens: &[Token]) -> bool {
    let word = |t: Option<&Token>| -> Option<String> {
        match t {
            Some(Token::Ident(n)) => Some(n.to_ascii_uppercase()),
            Some(Token::Keyword(k)) => Some(format!("{k:?}").to_ascii_uppercase()),
            _ => None,
        }
    };
    (0..tokens.len()).any(|i| {
        word(tokens.get(i)).as_deref() == Some("FOR")
            && matches!(
                word(tokens.get(i + 1)).as_deref(),
                Some("UPDATE" | "SHARE" | "NO" | "KEY")
            )
    })
}

/// `(` の直前の語が構文上の語（`IN`・`AS`・`OVER` 等）でも組み込み関数名でもない呼び出しを含むか
/// （ユーザー定義関数の呼び出しの保守的な検出。構文語の取りこぼしは `Write` 側へ倒れるだけ）。
fn calls_non_builtin_function(tokens: &[Token]) -> bool {
    (0..tokens.len()).any(|i| is_non_builtin_call_at(tokens, i))
}

/// 位置 `i` が、構文語でも組み込み関数でもない呼び出し（`名前(`・`schema.名前(`）の名前か。構文位置と
/// 確認できない語は呼び出し扱い（`true`。fail-closed）。[`calls_non_builtin_function`] の 1 位置分で、
/// `core::standard_body_binding` が標準形の本体の関数依存を集めるときにも共有する（`IN (`・`VALUES (`・
/// `EXISTS (SELECT` のような構文語を同名の永続関数への依存として記録しないため。Issue #2219）。
pub(crate) fn is_non_builtin_call_at(tokens: &[Token], i: usize) -> bool {
    let Some(t) = tokens.get(i) else {
        return false;
    };
    // 構文上 `(` の直前に現れない語（`END`・`NULL`・`TRUE`・`FALSE`・`DEFAULT`）は含めない。これらは
    // 予約されない名前として同名のユーザー関数がありうるため、`(` が続けば呼び出し（`Write`）とする。
    const SYNTAX_WORDS: &[&str] = &[
        "IN",
        "AS",
        "ON",
        "OR",
        "NOT",
        "ANY",
        "ALL",
        "SOME",
        "EXISTS",
        "VALUES",
        "USING",
        "WHEN",
        "THEN",
        "ELSE",
        "CASE",
        "BETWEEN",
        "LIKE",
        "ILIKE",
        "DISTINCT",
        "WITH",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "OFFSET",
        "GROUP",
        "HAVING",
        "TO",
        "IS",
        "ARRAY",
        "LATERAL",
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "NATURAL",
        "COLLATE",
        "CAST",
        "EXTRACT",
        "FROM",
        "BY",
        "UNNEST",
        "JSON_EXISTS",
        "JSON_VALUE",
        "JSON_QUERY",
        "ROW",
        "TRIM",
        "POSITION",
        "OVERLAY",
        "SUBSTRING",
        "ROW_NUMBER",
        "RANK",
        "DENSE_RANK",
        "PERCENT_RANK",
        "CUME_DIST",
        "NTILE",
        "LAG",
        "LEAD",
        "FIRST_VALUE",
        "LAST_VALUE",
        "NTH_VALUE",
    ];
    // 型名と同名のユーザー関数がありうる語。直前が `::`・`AS`（型修飾子の位置）と確認できる場合に
    // 限り構文語として除外し、確認できなければ呼び出し（`Write`）とする。
    const TYPE_WORDS: &[&str] = &[
        "NUMERIC",
        "DECIMAL",
        "VARCHAR",
        "CHAR",
        "CHARACTER",
        "BPCHAR",
        "VECTOR",
        "TIMESTAMP",
        "TIMESTAMPTZ",
        "TIME",
        "TIMETZ",
        "INTERVAL",
        "BIT",
        "VARBIT",
        "FLOAT",
    ];
    // 同名のユーザー関数がありうる構文語（位置で構文かどうかを確認する語）。
    const POSITIONAL_SYNTAX_WORDS: &[&str] = &[
        "LATERAL", "TO", "JOIN", "INNER", "CROSS", "NATURAL", "FULL", "LEFT", "RIGHT",
    ];
    // 直後に式（関数呼び出し）が来る語。直前がこれらなら `(` 直前の語は式の先頭＝呼び出し形。
    const EXPR_PREFIX_WORDS: &[&str] = &[
        "ZONE",
        "ESCAPE",
        "TO",
        "JOIN",
        "LATERAL",
        "DISTINCT",
        "ALL",
        "AND",
        "OR",
        "NOT",
        "WHEN",
        "THEN",
        "ELSE",
        "CASE",
        "BY",
        "IN",
        "IS",
        "LIKE",
        "ILIKE",
        "BETWEEN",
        "SELECT",
        "WHERE",
        "ON",
        "HAVING",
        "OFFSET",
        "LIMIT",
        "RETURNING",
        "SET",
        "VALUES",
        "DEFAULT",
        "FROM",
        "USING",
        "AS",
    ];
    {
        // `schema.func(...)`（修飾付き呼び出し）は構文語になりえないため常に呼び出しとして扱う。
        if matches!(t, Token::QualifiedIdent { .. })
            && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('(')))
        {
            return true;
        }
        let (Token::Ident(name), Some(Token::Punct('('))) = (t, tokens.get(i.saturating_add(1)))
        else {
            return false;
        };
        // `USING PLAN(...)` の `PLAN` は直前が `USING` のときだけ構文語。それ以外の `plan(...)`、
        // および `ERROR`・`EMPTY`・`KEYS` 等の予約されない名前は同名のユーザー関数でありうるため
        // 構文語として除外せず、呼び出しとして扱う（`Write` 側＝fail-closed）。
        let is_using_plan = name.eq_ignore_ascii_case("PLAN")
            && i > 0
            && matches!(
                tokens.get(i - 1),
                Some(Token::Ident(a)) if a.eq_ignore_ascii_case("USING")
            );
        // 予約されない名前（`materialized`・`filter`・`over` 等）は同名のユーザー関数でありうる。
        // 直前のトークンで構文位置（`AS|NOT MATERIALIZED (`・`) FILTER (`・`) OVER (`）と
        // 確認できる場合に限り構文語として除外し、確認できなければ呼び出し（`Write`）とする。
        let prev_ident_is = |words: &[&str]| {
            i > 0
                && matches!(
                    tokens.get(i - 1),
                    Some(Token::Ident(a)) if words.iter().any(|w| a.eq_ignore_ascii_case(w))
                )
        };
        let prev_is_close = i > 0 && matches!(tokens.get(i - 1), Some(Token::Punct(')')));
        let is_contextual_syntax = (name.eq_ignore_ascii_case("MATERIALIZED")
            && prev_ident_is(&["AS", "NOT"]))
            || ((name.eq_ignore_ascii_case("FILTER") || name.eq_ignore_ascii_case("OVER"))
                && prev_is_close);
        let is_type_modifier = TYPE_WORDS.iter().any(|k| name.eq_ignore_ascii_case(k))
            && i > 0
            && match tokens.get(i - 1) {
                Some(Token::TypeCast) => true,
                Some(Token::Ident(a)) => a.eq_ignore_ascii_case("AS"),
                _ => false,
            };
        // 型付きリテラル `TIMESTAMP(3) '...'`（修飾子の閉じ括弧の直後が文字列リテラル）も型修飾子。
        let is_typed_literal = TYPE_WORDS.iter().any(|k| name.eq_ignore_ascii_case(k)) && {
            let mut depth = 0usize;
            let mut after = None;
            for (j, tk) in tokens.iter().enumerate().skip(i.saturating_add(1)) {
                match tk {
                    Token::Punct('(') => depth = depth.saturating_add(1),
                    Token::Punct(')') => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            after = tokens.get(j.saturating_add(1));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            matches!(after, Some(Token::StringLiteral(_)))
        };
        // `LATERAL`・`TO`・`JOIN` 系は `is_reserved_function_name` に含まれず、同名のユーザー関数
        // （`lateral()` 等）を定義・呼び出せる。構文位置と確認できる場合に限り構文語として除外し、
        // 確認できなければ呼び出し（`Write`＝fail-closed）とする。
        //   - `LATERAL (` は直後が `SELECT`／`WITH`／`VALUES`（副問い合わせ）のときだけ構文
        //   - `JOIN`・`TO` 等は直前が式の終端（識別子・`)`）で、式演算子語でないときだけ構文
        let is_contextual_word = POSITIONAL_SYNTAX_WORDS
            .iter()
            .any(|k| name.eq_ignore_ascii_case(k));
        let positional_confirmed = if name.eq_ignore_ascii_case("LATERAL") {
            matches!(
                tokens.get(i.saturating_add(2)),
                Some(Token::Keyword(Keyword::Select))
            ) || matches!(
                tokens.get(i.saturating_add(2)),
                Some(Token::Ident(a))
                    if a.eq_ignore_ascii_case("WITH") || a.eq_ignore_ascii_case("VALUES")
            )
        } else {
            i > 0
                && match tokens.get(i - 1) {
                    Some(Token::Punct(')')) | Some(Token::QualifiedIdent { .. }) => true,
                    Some(Token::Ident(a)) => {
                        !EXPR_PREFIX_WORDS.iter().any(|w| a.eq_ignore_ascii_case(w))
                    }
                    _ => false,
                }
        };
        // 上記以外の構文語（`ON`・`IN`・`OR`・`NOT`・`EXISTS`・`VALUES`・`WHEN`・`EXTRACT`・窓関数名 等）も
        // 予約語ではなく、同名のユーザー関数を定義して式の先頭位置（`SELECT on()` 等）から呼べる。
        // 位置・形から構文と確認できる場合に限り除外し、確認できなければ呼び出し（`Write`）とする。
        let is_syntax_word = SYNTAX_WORDS.iter().any(|k| name.eq_ignore_ascii_case(k))
            && if is_contextual_word {
                positional_confirmed
            } else {
                syntax_form_confirmed(tokens, i, name, EXPR_PREFIX_WORDS)
            };
        if is_using_plan
            || is_contextual_syntax
            || is_type_modifier
            || is_typed_literal
            || is_syntax_word
            || crate::sql::udf_call::is_reserved_function_name(name)
        {
            return false;
        }
        // `AS 別名(列, ...)` は別名の列名リストで、呼び出しではない。
        if i > 0
            && matches!(
                tokens.get(i - 1),
                Some(Token::Ident(a)) if a.eq_ignore_ascii_case("AS")
            )
        {
            return false;
        }
        // `名前(列, ...) AS (` は CTE の列名リストで、呼び出しではない。
        let mut depth = 0usize;
        for (j, tk) in tokens.iter().enumerate().skip(i.saturating_add(1)) {
            match tk {
                Token::Punct('(') => depth = depth.saturating_add(1),
                Token::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        // CTE 定義は `AS [NOT] [MATERIALIZED] (` の形に限る。投影の別名
                        // `w() AS result` は後ろが `(` にならないため呼び出しとして扱う。
                        let mut k = j.saturating_add(1);
                        if !matches!(
                            tokens.get(k),
                            Some(Token::Ident(a)) if a.eq_ignore_ascii_case("AS")
                        ) {
                            return true;
                        }
                        k = k.saturating_add(1);
                        if matches!(
                            tokens.get(k),
                            Some(Token::Ident(a)) if a.eq_ignore_ascii_case("NOT")
                        ) {
                            k = k.saturating_add(1);
                        }
                        if matches!(
                            tokens.get(k),
                            Some(Token::Ident(a)) if a.eq_ignore_ascii_case("MATERIALIZED")
                        ) {
                            k = k.saturating_add(1);
                        }
                        return !matches!(tokens.get(k), Some(Token::Punct('(')));
                    }
                }
                _ => {}
            }
        }
        true
    }
}

/// `COPY` 文のトークン列を [`StatementEffect`] へ分類する（[`classify_statement`] の
/// 補助）。括弧の深さ 0 の位置で先頭の `COPY` の後に最初に現れる `TO` なら
/// `ReadOnly`、それ以外（`FROM`・どちらも無い）は `Write`。
/// `COPY (SELECT ... FROM t) TO STDOUT` の内側の `FROM` は深さ 1 なので無視する。
/// `COPY` の直後がテーブル名（`(` 以外）の場合、その 1 トークンは方向語の候補から
/// 除外する（引用符なしの `to` という名前のテーブルへの `COPY to FROM STDIN` を
/// 読み取り専用と誤判定しない。誤判定は後続文失敗時の書き込み永続化を招くため、
/// 曖昧な形は `Write` 側（fail-closed）に倒す）。
fn classify_copy(tokens: &[Token]) -> StatementEffect {
    let mut depth: usize = 0;
    let skip = match tokens.get(1) {
        Some(Token::Punct('(')) | None => 1,
        Some(_) => 2,
    };
    for token in tokens.iter().skip(skip) {
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
    // Issue #1129: 2 文以上のメッセージに分割実行が含まれていれば全体を拒否する
    // （メッセージ内の位置・セッション状態によらず。何も実行しない）。
    if stmts.len() >= 2 && stmts.iter().any(|s| is_partitioned_statement(s)) {
        return Err(MultiStatementError::PartitionedInMultiStatement);
    }
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

/// 1 文が分割実行（`USING OPERATION_ID ... PARTITIONED` 修飾、または `CANCEL PARTITIONED`）か。
/// 字句解析に失敗する文は `false`（実行されても許可リスト外で拒否され副作用がない）。
fn is_partitioned_statement(stmt: &str) -> bool {
    match tokenize(stmt) {
        Ok(tokens) => {
            crate::sql::partitioned::tokens_have_partitioned_clause(&tokens)
                || crate::sql::partitioned::tokens_are_partitioned_cancel(&tokens)
        }
        Err(_) => false,
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
            StatementEffect::TransactionControl(
                TxnControl::Begin | TxnControl::StartTransaction,
            ) => {
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

    // 識別子位置の `BEGIN`（引数名・列名）は本体の深さに数えない（codex 指摘・P1）。
    #[test]
    fn begin_as_identifier_does_not_unbalance_atomic_body() {
        let f = "CREATE FUNCTION f(begin int) RETURNS int BEGIN ATOMIC SELECT begin; END";
        let both = format!("{f}; SELECT f(1)");
        match split_statements(&both).expect("split") {
            SplitOutcome::Statements(v) => assert_eq!(v, vec![f, "SELECT f(1)"]),
            other => panic!("expected Statements, got {other:?}"),
        }
    }

    // Issue #2219（SQL-67 (2)）: `BEGIN ATOMIC` 本体の内側の `;` では分割しない。
    #[test]
    fn begin_atomic_body_is_not_split() {
        let f = "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC SELECT x; SELECT x + 1; END";
        assert_eq!(split_statements(f).expect("split"), SplitOutcome::Single);
        let both = format!("{f}; SELECT f(1)");
        assert_eq!(
            statements(split_statements(&both).expect("split")),
            vec![f, "SELECT f(1)"]
        );
        let with_semi = format!("{f};");
        assert_eq!(
            split_statements(&with_semi).expect("split"),
            SplitOutcome::Single
        );
    }

    #[test]
    fn begin_atomic_tracks_case_end_nesting() {
        let f = "create function f(x int) returns int begin atomic \
                 select case when x > 0 then 1 else 2 end; end";
        let both = format!("{f}; SELECT 1");
        assert_eq!(
            statements(split_statements(&both).expect("split")),
            vec![f, "SELECT 1"]
        );
    }

    #[test]
    fn unterminated_begin_atomic_is_single() {
        let f = "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; SELECT 2";
        assert_eq!(split_statements(f).expect("split"), SplitOutcome::Single);
    }

    #[test]
    fn begin_atomic_in_quotes_or_comments_is_ignored_and_tx_begin_unchanged() {
        let q = "SELECT 'BEGIN ATOMIC'; SELECT 2";
        assert_eq!(
            statements(split_statements(q).expect("split")),
            vec!["SELECT 'BEGIN ATOMIC'", "SELECT 2"]
        );
        let c = "SELECT 1 /* CREATE BEGIN ATOMIC */; SELECT 2";
        assert_eq!(statements(split_statements(c).expect("split")).len(), 2);
        let tx = "BEGIN; SELECT 1; COMMIT";
        assert_eq!(
            statements(split_statements(tx).expect("split")),
            vec!["BEGIN", "SELECT 1", "COMMIT"]
        );
        let create_then_begin = "CREATE FUNCTION f() RETURNS int AS 1; BEGIN ATOMIC; SELECT 1";
        assert_eq!(
            statements(split_statements(create_then_begin).expect("split")).len(),
            3
        );
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
    fn comments_and_quoted_identifiers_are_split_lexically_and_lexer_accepts_pieces() {
        // コメント・引用内の `;` は区切りにならず、外側の `;` では分割される。
        // 断片は加工されず、lexer が各断片を受理する（Issue #1346）。
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
                "SELECT 'a;b' /* ; */ ; SELECT \"x\"",
                vec!["SELECT 'a;b' /* ; */", "SELECT \"x\""],
            ),
            (
                "SELECT \"a\" FROM \"t\"; SELECT 2",
                vec!["SELECT \"a\" FROM \"t\"", "SELECT 2"],
            ),
            ("SELECT 1; /* c */; SELECT 2", vec!["SELECT 1", "SELECT 2"]),
        ];
        for (input, expected) in cases {
            let pieces = statements(split_statements(input).expect("split"));
            assert_eq!(pieces, expected, "input: {input}");
            for p in &pieces {
                assert!(tokenize(p).is_ok(), "lexer must accept {p:?} of: {input}");
            }
        }
    }

    #[test]
    fn quoted_identifier_with_unsafe_content_is_split_but_rejected_by_lexer() {
        // 領域の終端は決まるので分割は行い、その文だけが lexer で拒否される。
        let pieces =
            statements(split_statements("SELECT \"a;b\" FROM t; SELECT 2").expect("split"));
        assert_eq!(pieces, vec!["SELECT \"a;b\" FROM t", "SELECT 2"]);
        assert!(tokenize(pieces[0]).is_err());
        assert!(tokenize(pieces[1]).is_ok());
    }

    #[test]
    fn comment_only_fragments_are_empty_statements() {
        assert_eq!(
            split_statements("SELECT 1; -- done").expect("split"),
            SplitOutcome::Single
        );
        assert_eq!(
            split_statements("SELECT 1 -- c; SELECT 2").expect("split"),
            SplitOutcome::Single
        );
        for input in ["-- only", ";-- c", "/* x */ ;", "/* a */ -- b\n ;; /* c */"] {
            assert_eq!(
                split_statements(input).expect("split"),
                SplitOutcome::Empty,
                "input: {input}"
            );
        }
    }

    #[test]
    fn comment_prefix_does_not_hide_write_statements() {
        let pieces = statements(
            split_statements("/* x */ INSERT INTO t VALUES (1); SELECT 1").expect("split"),
        );
        assert_eq!(
            check_write_placement(&pieces, false),
            Err(MultiStatementError::WriteNotLast)
        );
        assert_eq!(
            classify_statement("-- c\nINSERT INTO t VALUES (1)"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("/* c */ SELECT 1"),
            StatementEffect::ReadOnly
        );
    }

    #[test]
    fn dollar_quoted_bodies_do_not_split() {
        for (input, first) in [
            ("SELECT $$a;b$$; SELECT 2", "SELECT $$a;b$$"),
            ("SELECT $t$ ; $t$; SELECT 2", "SELECT $t$ ; $t$"),
            ("SELECT $$ -- ' /* $$; SELECT 2", "SELECT $$ -- ' /* $$"),
        ] {
            match split_statements(input).expect("split") {
                SplitOutcome::Statements(v) => {
                    assert_eq!(v.len(), 2, "{input}");
                    assert_eq!(v.first().map(|s| s.trim()), Some(first), "{input}");
                }
                other => panic!("expected Statements for {input}: {other:?}"),
            }
            assert!(tokenize(first).is_ok(), "{first}");
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
            "SELECT $$a; SELECT 2",
            "SELECT a$$x$$; SELECT 2",
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
        // コメントだけの断片は空文として落ち、`INSERT` は切り出されない。
        assert_eq!(pieces, vec!["SELECT 1", "SELECT 2"]);
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

    /// Issue #1930: `START TRANSACTION`／`END`／`ABORT` は制御文として分類され、
    /// `TRANSACTION` を伴わない `START ...` は `Write`（fail-closed）に倒れる。
    #[test]
    fn transaction_aliases_are_classified_as_control_statements() {
        use crate::sql::transaction::TxnControl;
        assert_eq!(
            classify_statement("START TRANSACTION"),
            StatementEffect::TransactionControl(TxnControl::StartTransaction)
        );
        assert_eq!(
            classify_statement("END"),
            StatementEffect::TransactionControl(TxnControl::Commit)
        );
        assert_eq!(
            classify_statement("abort"),
            StatementEffect::TransactionControl(TxnControl::Rollback)
        );
        assert_eq!(classify_statement("START foo"), StatementEffect::Write);
        let ins = "INSERT INTO t (id) VALUES (1) USING OPERATION_ID 'o1'";
        let ins2 = "INSERT INTO t (id) VALUES (2) USING OPERATION_ID 'o2'";
        assert_eq!(
            plan_multi_statement(&["START TRANSACTION", ins, ins2, "END"], false),
            Ok(MultiStatementPlan::Sequential)
        );
        assert_eq!(
            plan_multi_statement(&["START TRANSACTION", ins, "END", ins2], false),
            Err(MultiStatementError::WriteNotLast)
        );
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
        // 引用符なし識別子 `to` をテーブル名に持つ COPY FROM は書き込み扱い。
        assert_eq!(
            classify_statement("COPY to FROM STDIN USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("COPY to (id) FROM STDIN USING OPERATION_ID 'o1'"),
            StatementEffect::Write
        );
        assert_eq!(
            classify_statement("COPY to TO STDOUT"),
            StatementEffect::ReadOnly
        );
        // テーブル形 TO STDOUT（Issue #1771）は列リストの有無によらず読み取り扱い。
        assert_eq!(
            classify_statement("COPY t TO STDOUT"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("COPY t (a, b) TO STDOUT WITH (FORMAT csv)"),
            StatementEffect::ReadOnly
        );
    }

    #[test]
    fn row_lock_select_is_classified_as_write_and_implicit_txn() {
        for sql in [
            "SELECT id FROM t FOR UPDATE",
            "SELECT id FROM t FOR SHARE NOWAIT",
            "SELECT id FROM t FOR NO KEY UPDATE",
            "SELECT id FROM t FOR KEY SHARE",
            "WITH x AS (SELECT id FROM t) SELECT * FROM x FOR UPDATE",
        ] {
            assert_eq!(classify_statement(sql), StatementEffect::Write, "{sql}");
        }
        assert_eq!(
            classify_statement("SELECT id FROM t"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            plan_multi_statement(
                &["SELECT id FROM t FOR UPDATE", "UPDATE t SET v = 1"],
                false
            )
            .expect("plan"),
            MultiStatementPlan::ImplicitTransaction
        );
    }

    #[test]
    fn multibyte_characters_in_literals_do_not_break_offsets() {
        let outcome = split_statements("SELECT 1 WHERE body = 'あ;い'; SELECT 2").expect("split");
        assert_eq!(
            statements(outcome),
            vec!["SELECT 1 WHERE body = 'あ;い'", "SELECT 2"]
        );
    }

    /// Issue #2218: 組み込みだけの `SELECT` は読み取り、ユーザー定義関数の呼び出しを含めば書き込み
    /// 扱い（`LANGUAGE sql|plpgsql` の本体が書きうるため）。構文語の括弧は呼び出しと誤認しない。
    #[test]
    fn classify_statement_treats_user_defined_calls_as_writes() {
        for sql in [
            "SELECT 1",
            "SELECT abs(1), upper('a')",
            "SELECT count(*) FROM t",
            "SELECT a FROM t WHERE a IN (1, 2)",
            "SELECT CAST(1 AS int)",
            "SELECT a FROM t WHERE NOT (a = 1)",
            "WITH x AS (SELECT id FROM t) SELECT * FROM x LIMIT 1",
            "SELECT id FROM t USING PLAN('find the invoices')",
            "SELECT coalesce(a, 1), nullif(a, 2), greatest(a, 1), least(a, 1) FROM t",
            "SELECT date_trunc('day', ts), date_part('year', ts), now() FROM t",
            "SELECT timezone('UTC', ts), extract(year FROM ts) FROM t",
            "SELECT id FROM t AS x(a, b)",
            "SELECT CAST(a AS numeric(10, 2)), a::varchar(5), b::vector(3) FROM t",
            "SELECT a FROM t WHERE a BETWEEN 1 AND (2)",
            "SELECT row_number() OVER (ORDER BY a) FROM t",
            "SELECT array_length(arr, 1), jsonb_build_object('a', 1) FROM t",
            "SELECT trim(b), position('a' IN b), overlay(b PLACING 'x' FROM 1) FROM t",
            "SELECT TIMESTAMP(3) '2024-01-01 00:00:00'",
            "WITH x AS MATERIALIZED (SELECT id FROM t) SELECT * FROM x",
            "WITH x AS NOT MATERIALIZED (SELECT id FROM t) SELECT * FROM x",
            "SELECT count(*) FILTER (WHERE a > 1) FROM t",
            "SELECT a FROM t JOIN (SELECT a FROM u) x ON t.a = x.a",
            "SELECT a FROM t, LATERAL (SELECT a FROM u) x",
            "SELECT a FROM t JOIN u ON (t.a = u.a)",
            "SELECT a FROM t WHERE a NOT IN (1, 2) OR (a = 3)",
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u) AND NOT (a = 1)",
            "SELECT CASE WHEN (a = 1) THEN (2) ELSE (3) END FROM t",
            "SELECT a FROM t UNION (SELECT a FROM u)",
            "SELECT a FROM t JOIN u USING (a)",
            "SELECT a FROM t GROUP BY a HAVING (count(*) > 1)",
            "SELECT a FROM t LIMIT 1 OFFSET (1)",
            "SELECT ntile(2) OVER (ORDER BY a), lag(a) OVER (ORDER BY a) FROM t",
        ] {
            assert_eq!(classify_statement(sql), StatementEffect::ReadOnly, "{sql}");
        }
        for sql in [
            "SELECT s.f()",
            "SELECT 1 FROM t WHERE s.f(a) = 1",
            "SELECT end()",
            "SELECT null(), true(), false(), default()",
            "SELECT now() AT TIME ZONE to(1)",
            "SELECT now() AT TIME ZONE join(1)",
            "SELECT a FROM t WHERE a LIKE 'x' ESCAPE to(1)",
            "SELECT numeric()",
            "SELECT varchar(1)",
            "SELECT 1/0, vector(1)",
            "SELECT my_fn(1)",
            "SELECT abs(my_fn(1))",
            "SELECT a FROM t WHERE my_fn(a) = 1",
            "WITH x AS (SELECT my_fn(1)) SELECT * FROM x",
            "SELECT w() AS result",
            "SELECT error()",
            "SELECT empty(1), keys(2)",
            "SELECT a FROM t USING PLAN(w())",
            "SELECT plan(1)",
            "SELECT materialized()",
            "SELECT filter(1), over(2), search(3), rows(4)",
            "SELECT returning()",
            "SELECT window(1)",
            "SELECT lateral()",
            "SELECT 1, lateral(1)",
            "SELECT a FROM t WHERE to(a) = 1",
            "SELECT DISTINCT join()",
            "SELECT a FROM t WHERE a > 0 AND left_()",
            "SELECT on()",
            "SELECT \"on\"()",
            "SELECT in(1), or(1), is(1), like(1), ilike(1), between(1)",
            "SELECT a FROM t WHERE a = on(1)",
            "SELECT not(1), exists(1), values(1), when(1), then(1), else(1)",
            "SELECT distinct(1), row(1), unnest(1), with(1), union(1), offset(1)",
            "SELECT extract(1), overlay(1), row_number(), rank(), lag(1)",
            "SELECT using(1), group(1), having(1), collate(1), intersect(1), except(1)",
            "SELECT a FROM t WHERE a = 1 AND in(1)",
        ] {
            assert_eq!(classify_statement(sql), StatementEffect::Write, "{sql}");
        }
    }

    /// 引用識別子の構文語名（`"from"`・`"by"`）は ReadOnly にならない。現状は lexer が
    /// `Rejected` へ落とすが、将来受理されても呼び出し（`Write`）側に倒れる契約を固定する。
    #[test]
    fn quoted_syntax_word_calls_are_never_read_only() {
        for sql in ["SELECT \"from\"()", "SELECT \"by\"()", "SELECT \"on\"()"] {
            assert_ne!(classify_statement(sql), StatementEffect::ReadOnly, "{sql}");
        }
    }

    /// SQL-67・Issue #2220: FROM 句の関数呼び出し（本体が書き込みうる）は `Write` に分類する
    /// （複数文メッセージの書き込み配置制約をすり抜けない）。
    #[test]
    fn function_in_from_is_classified_as_write() {
        for sql in [
            "SELECT * FROM f(1)",
            "SELECT a FROM f(1) AS t(a) WHERE a > 1",
            "SELECT count(*) FROM g()",
        ] {
            assert_eq!(classify_statement(sql), StatementEffect::Write, "{sql}");
        }
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
        // Issue #1194: ENUM 型 DDL も書き込み系（カタログを commit する）。
        assert_eq!(
            classify_statement("CREATE TYPE t AS ENUM ('a')"),
            StatementEffect::Write
        );
        assert_eq!(classify_statement("DROP TYPE t"), StatementEffect::Write);
        // TASK-213・SQL-29 (b)（Issue #928）: `WITH`（非再帰 CTE）は読み取り系
        // （`sql::allowlist::validate_sql_tokens` の `WITH` 分岐が受理する形は
        // 主クエリが広域取得のみで、書き込みを持たない）。
        assert_eq!(
            classify_statement("WITH x AS (SELECT id FROM t) SELECT * FROM x LIMIT 1"),
            StatementEffect::ReadOnly
        );
        // 列名リストの `delete` は本文ではない（読み取り分類のまま）。
        assert_eq!(
            classify_statement(
                "WITH x(delete) AS (SELECT id FROM documents) SELECT * FROM x LIMIT 1"
            ),
            StatementEffect::ReadOnly
        );
        // SQL-63・Issue #1875: データ変更 CTE は書き込み系（`[NOT] MATERIALIZED` を挟む形を含む）。
        for sql in [
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d LIMIT 1",
            "WITH d AS MATERIALIZED (UPDATE t SET a = 'x' RETURNING id) SELECT * FROM d LIMIT 1",
            "WITH d AS NOT MATERIALIZED (INSERT INTO t (a) VALUES ('x')) SELECT * FROM t LIMIT 1",
            "WITH s AS (SELECT id FROM t), d AS (delete from t) SELECT * FROM s LIMIT 1",
        ] {
            assert_eq!(classify_statement(sql), StatementEffect::Write, "sql={sql}");
        }
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

    // --- 分割実行 DML（Issue #1129）---------------------------------------------

    const PARTITIONED_DELETE: &str =
        "DELETE FROM t WHERE n > 0 USING OPERATION_ID 'op' PARTITIONED CHUNK 2";

    #[test]
    fn partitioned_statement_in_a_multi_statement_message_is_rejected_at_any_position() {
        for stmts in [
            vec![PARTITIONED_DELETE, "SELECT 1"],
            vec!["SELECT 1", PARTITIONED_DELETE],
            vec!["SELECT 1", "CANCEL PARTITIONED DML 'op' ON t"],
            vec!["BEGIN", PARTITIONED_DELETE, "COMMIT"],
        ] {
            for in_txn in [false, true] {
                assert_eq!(
                    plan_multi_statement(&stmts, in_txn),
                    Err(MultiStatementError::PartitionedInMultiStatement),
                    "{stmts:?}"
                );
            }
        }
        assert_eq!(
            MultiStatementError::PartitionedInMultiStatement.error_class(),
            ErrorClass::ActiveSqlTransaction
        );
        assert_eq!(
            MultiStatementError::PartitionedInMultiStatement.client_message(),
            "partitioned DML cannot run inside a transaction block"
        );
    }

    #[test]
    fn a_column_or_table_named_partitioned_is_not_detected() {
        for stmt in [
            "DELETE FROM partitioned WHERE partitioned = 1 USING OPERATION_ID 'op'",
            "UPDATE t SET partitioned = 'x' WHERE n > 0 USING OPERATION_ID 'op'",
            "SELECT partitioned FROM t",
        ] {
            assert!(!is_partitioned_statement(stmt), "{stmt}");
        }
        assert!(is_partitioned_statement(PARTITIONED_DELETE));
        assert!(is_partitioned_statement(
            "UPDATE t SET n = 1 WHERE n > 0 USING OPERATION_ID NULL PARTITIONED"
        ));
        assert!(is_partitioned_statement("cancel partitioned dml 'x' on t"));
        // 字句解析に失敗する文は対象外（実行されても許可リスト外で拒否される）。
        assert!(!is_partitioned_statement(
            "DELETE FROM t WHERE x = 'unterminated"
        ));
        // 単一の分割実行文は複数文ではないので拒否されない。
        assert_eq!(
            plan_multi_statement(&[PARTITIONED_DELETE], false),
            Ok(MultiStatementPlan::Sequential)
        );
    }

    #[test]
    fn show_is_read_only_and_cancel_is_a_write() {
        assert_eq!(
            classify_statement("SHOW PARTITIONED DML 'x' ON t"),
            StatementEffect::ReadOnly
        );
        assert_eq!(
            classify_statement("CANCEL PARTITIONED DML 'x' ON t"),
            StatementEffect::Write
        );
    }
}
