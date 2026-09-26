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
//! [`lexer::tokenize`][crate::sql::lexer::tokenize] が拒否する構文（SQL コメント
//! `--`・`/* */`、二重引用符識別子、未終端の文字列リテラル）をリテラル外で
//! 検出した場合は分割せず [`SplitOutcome::Single`] を返し、元テキストをそのまま
//! 呼び出し元へ渡す。これにより「コメントや未終端引用符で区切りを隠し、
//! 後続の文を密輸する」経路を構造的に塞ぐ（分割器の拒否規則と lexer の拒否規則の
//! 連動を単体テストで固定する）。文字列リテラル（`'...'`。`''` エスケープを含む）
//! 内の `;` は区切りとみなさない。
//!
//! # 原子性（暗黙トランザクション）の扱い
//!
//! 明示 `BEGIN`／複数文単位のトランザクション機構（SQL-31・RECOVER-12）が
//! 未実装の現状では、書き込み系文（[`StatementEffect::Write`]）を含む複数文
//! メッセージのうち「書き込みが最後の 1 文に限られる」形のみを受理する。
//! この制約下では、先行文がエラーになれば書き込み文はまだ実行されておらず、
//! 最後の書き込み文自身がエラーになればその文の redb トランザクションが
//! 単独で原子的に失敗するため、追加の分散トランザクション機構なしに WIRE-16 の
//! 原子性要件が構造的に成立する。書き込みが最後以外にある場合は
//! [`MultiStatementError::WriteNotLast`]（`0A000`）で 1 文も実行せずに拒否する。
//! SQL-31・RECOVER-12 実装後にこの制約を緩める判断は別途行う
//! （`docs/design/wire-multi-statement.md` 参照）。

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
    /// ある等）。各要素は `;` を含まず前後の空白を trim 済み。
    Statements(Vec<&'a str>),
}

/// 複数文実行の分割・配置検証エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiStatementError {
    /// 非空文の数が [`MAX_STATEMENTS_PER_QUERY`] を超過した（`54000`）。
    TooManyStatements,
    /// 書き込み系文（[`StatementEffect::Write`]）が最後の文以外の位置にある
    /// （`0A000`）。本モジュールのドキュメント「原子性」節参照。
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
                "write statements (INSERT/UPDATE/DELETE/TRUNCATE) are only supported \
                 as the last statement in a multi-statement query"
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

/// SQL テキストをセミコロン区切りの文へ分割する。文字列リテラルの中にある `;`
/// では分割せず、lexer が拒否する構文（コメント・二重引用符識別子・未終端
/// リテラル）を検出した場合は分割せず [`SplitOutcome::Single`] を返す
/// （モジュールドキュメント参照）。
///
/// untrusted 入力経路のため `char_indices` と `str::get` のみを使い、
/// `unwrap`/`expect`/添字アクセスは使わない（`.claude/rules/coding-rust.md`）。
pub fn split_statements(input: &str) -> Result<SplitOutcome<'_>, MultiStatementError> {
    let mut in_literal = false;
    let mut chars = input.char_indices().peekable();
    // 1 パスで非空文を直接切り出す（`;` の位置をいったん `Vec<usize>` へ
    // 集めてから 2 パス目で切り出す方式は、`;;;;...` のような入力で
    // 「区切りだけの無制限 `Vec` 確保」に相当し untrusted 入力経路の防御的
    // 上限（`.claude/rules/coding-rust.md`「無制限リソース確保」対応）と
    // 相性が悪いため避ける。`statements` は上限到達時点で即座に `Err` を
    // 返すため `MAX_STATEMENTS_PER_QUERY + 1` 要素までしか伸びない）。
    let mut statements: Vec<&str> = Vec::new();
    let mut semicolon_count: usize = 0;
    let mut start = 0usize;

    while let Some(&(offset, c)) = chars.peek() {
        if in_literal {
            if c == '\'' {
                // `''` はエスケープ（リテラル継続）。`lexer::lex_string_literal`
                // と同じ規則。
                let mut lookahead = chars.clone();
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '\''))) {
                    lookahead.next();
                    chars = lookahead;
                    continue;
                }
                in_literal = false;
                chars.next();
                continue;
            }
            chars.next();
            continue;
        }

        match c {
            '\'' => {
                in_literal = true;
                chars.next();
            }
            '"' => {
                // 二重引用符識別子は lexer が無条件で拒否する構文。分割せず
                // 元テキスト全体を engine へ渡し、同一の `42601` を返させる。
                return Ok(SplitOutcome::Single);
            }
            '-' => {
                let mut lookahead = chars.clone();
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '-'))) {
                    // SQL コメント。コメント内の `;` を区切りとみなさないため
                    // 分割せず元テキストを渡す。
                    return Ok(SplitOutcome::Single);
                }
                chars.next();
            }
            '/' => {
                let mut lookahead = chars.clone();
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '*'))) {
                    return Ok(SplitOutcome::Single);
                }
                chars.next();
            }
            ';' => {
                let piece = input.get(start..offset).unwrap_or("").trim();
                if !piece.is_empty() {
                    statements.push(piece);
                    if statements.len() > MAX_STATEMENTS_PER_QUERY {
                        return Err(MultiStatementError::TooManyStatements);
                    }
                }
                semicolon_count = semicolon_count.saturating_add(1);
                // `;` は ASCII 1 バイトなので `offset + 1` は必ず次の文字境界。
                start = offset.saturating_add(1);
                chars.next();
            }
            _ => {
                chars.next();
            }
        }
    }

    if in_literal {
        // 未終端の文字列リテラル。`lexer::tokenize` が同一入力を必ず拒否するため
        // 分割せず元テキストを渡す（同一の `42601` に収束させる）。
        return Ok(SplitOutcome::Single);
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
        Token::Ident(_) => StatementEffect::Write,
        // `Select` 以外の `Keyword`（`From`/`Where`/`And`/`Order`/`By`/`Limit`）・
        // `Number`・`Punct`・`StringLiteral`・比較/距離演算子・`QualifiedIdent` は
        // いずれも `Token::Ident` を要求する書き込み分岐に到達できない先頭トークン
        // 形であり、必ず `validate_sql` の許可リスト外（`42601`）へ落ちる。
        _ => StatementEffect::Rejected,
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
    fn comments_double_quotes_and_unterminated_literals_are_single_and_lexer_agrees() {
        let inputs = [
            "SELECT 1 -- comment; SELECT 2",
            "SELECT 1 /* comment */; SELECT 2",
            "SELECT \"col\" FROM t; SELECT 2",
            "SELECT 'unterminated; SELECT 2",
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
