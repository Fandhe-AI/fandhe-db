//! `LOCK [TABLE] ...` の構文検証（SQL-38・Issue #1958）。
//!
//! 責務境界: トークン列の構造だけを検証して [`ValidatedLockTable`] を作る。カタログ照会（表の存在確認）と
//! 実行（writer 取得）は `core.rs` が担う（`DECLARE` と同じ設計。トランザクション外の判定〔`25P01`〕を
//! 名前の解決より先にするため、ここではカタログを引かない）。
//!
//! 単一ライタ（TABLE-3）のため 8 種のロックモードと表・行の粒度は区別せず、いずれも「トランザクションの
//! writer を取得して終了まで保持する」ことに写す（`docs/design/explicit-transaction.md`）。モードは
//! Describe・デバッグ・将来の拡張のために保持する。受信 SQL 由来のトークン列を扱うため、添字アクセス・
//! `unwrap` は使わない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;

/// `LOCK TABLE` のロックモード 8 種（SQL-38。実行では区別しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// `ACCESS SHARE`
    AccessShare,
    /// `ROW SHARE`
    RowShare,
    /// `ROW EXCLUSIVE`
    RowExclusive,
    /// `SHARE UPDATE EXCLUSIVE`
    ShareUpdateExclusive,
    /// `SHARE`
    Share,
    /// `SHARE ROW EXCLUSIVE`
    ShareRowExclusive,
    /// `EXCLUSIVE`
    Exclusive,
    /// `ACCESS EXCLUSIVE`（`IN ... MODE` 省略時の既定）
    AccessExclusive,
}

/// 構造検証済みの `LOCK [TABLE] ...`（core の `ParsedSql::Lock` が運ぶ）。
///
/// **BREAKING CHANGE**（Issue #1958）: 公開 enum `ParsedSql` への variant 追加で参照される。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLockTable {
    /// 対象の表名（文中の順・重複除去済み。解決器が内部キーへ置き換え済みの名前を含む）。
    pub table_names: Vec<String>,
    /// ロックモード（実行では区別しない）。
    pub mode: LockMode,
    /// `NOWAIT` 指定（他セッションが writer を保持していれば待たずに `55P03`）。
    pub nowait: bool,
}

/// 先頭語が `LOCK` か（`parse_tokens_in` の分岐判定）。
pub fn is_lock_statement(tokens: &[Token]) -> bool {
    is_word(tokens.first(), "LOCK")
}

fn is_word(t: Option<&Token>, kw: &str) -> bool {
    matches!(t, Some(Token::Ident(w)) if w.eq_ignore_ascii_case(kw))
}

fn malformed() -> SqlSurfaceError {
    SqlSurfaceError::unsupported("malformed LOCK statement")
}

fn parse_mode(words: &[String]) -> Option<LockMode> {
    let joined = words.join(" ").to_ascii_uppercase();
    Some(match joined.as_str() {
        "ACCESS SHARE" => LockMode::AccessShare,
        "ROW SHARE" => LockMode::RowShare,
        "ROW EXCLUSIVE" => LockMode::RowExclusive,
        "SHARE UPDATE EXCLUSIVE" => LockMode::ShareUpdateExclusive,
        "SHARE" => LockMode::Share,
        "SHARE ROW EXCLUSIVE" => LockMode::ShareRowExclusive,
        "EXCLUSIVE" => LockMode::Exclusive,
        "ACCESS EXCLUSIVE" => LockMode::AccessExclusive,
        _ => return None,
    })
}

fn skip(rest: &[Token]) -> &[Token] {
    rest.get(1..).unwrap_or(&[])
}

/// 名前解決・`search_path` ゲートより前にトランザクション外（`25P01`）を判定するための構造検証。
///
/// 解決前のトークン列は `schema.table` を `QualifiedIdent` で持つため、単一の名前へ畳んだ複製で
/// [`validate_lock_tokens`] と同じ検証を行う（結果は捨てる。カタログは引かない）。
pub(crate) fn validate_lock_structure_unresolved(tokens: &[Token]) -> Result<(), SqlSurfaceError> {
    let folded: Vec<Token> = tokens
        .iter()
        .map(|t| match t {
            Token::QualifiedIdent { qualifier, name } => {
                Token::Ident(format!("{qualifier}.{name}"))
            }
            other => other.clone(),
        })
        .collect();
    validate_lock_tokens(&folded).map(|_| ())
}

/// `LOCK [TABLE] [ONLY] <name> [*] [, ...] [IN <mode> MODE] [NOWAIT]` を構造検証する。
///
/// 許可形以外（未知のモード語・`MODE` 欠落・末尾の余分なトークン等）は `42601`（fail-closed）。
pub(crate) fn validate_lock_tokens(
    tokens: &[Token],
) -> Result<ValidatedLockTable, SqlSurfaceError> {
    let mut rest = tokens.get(1..).unwrap_or(&[]);
    if let Some((Token::Punct(';'), head)) = rest.split_last() {
        rest = head;
    }
    if is_word(rest.first(), "TABLE") {
        rest = skip(rest);
    }
    let mut table_names: Vec<String> = Vec::new();
    loop {
        if is_word(rest.first(), "ONLY") && matches!(rest.get(1), Some(Token::Ident(_))) {
            rest = skip(rest);
        }
        let Some((Token::Ident(name), tail)) = rest.split_first() else {
            return Err(malformed());
        };
        rest = tail;
        if !table_names.iter().any(|n| n == name) {
            table_names.push(name.clone());
        }
        if matches!(rest.first(), Some(Token::Punct('*'))) {
            rest = skip(rest);
        }
        if matches!(rest.first(), Some(Token::Punct(','))) {
            rest = skip(rest);
            continue;
        }
        break;
    }
    let mut mode = LockMode::AccessExclusive;
    if is_word(rest.first(), "IN") {
        rest = skip(rest);
        let mut words: Vec<String> = Vec::new();
        loop {
            match rest.split_first() {
                Some((Token::Ident(w), tail)) if w.eq_ignore_ascii_case("MODE") => {
                    rest = tail;
                    break;
                }
                Some((Token::Ident(w), tail)) if words.len() < 3 => {
                    words.push(w.clone());
                    rest = tail;
                }
                _ => return Err(malformed()),
            }
        }
        mode = parse_mode(&words).ok_or_else(malformed)?;
    }
    let mut nowait = false;
    if is_word(rest.first(), "NOWAIT") {
        nowait = true;
        rest = skip(rest);
    }
    if !rest.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "unsupported clause after LOCK statement",
        ));
    }
    Ok(ValidatedLockTable {
        table_names,
        mode,
        nowait,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn parse(sql: &str) -> Result<ValidatedLockTable, SqlSurfaceError> {
        validate_lock_tokens(&tokenize(sql).expect("lex"))
    }

    #[test]
    fn eight_modes_with_optional_table_and_nowait() {
        let modes = [
            ("ACCESS SHARE", LockMode::AccessShare),
            ("ROW SHARE", LockMode::RowShare),
            ("ROW EXCLUSIVE", LockMode::RowExclusive),
            ("SHARE UPDATE EXCLUSIVE", LockMode::ShareUpdateExclusive),
            ("SHARE", LockMode::Share),
            ("SHARE ROW EXCLUSIVE", LockMode::ShareRowExclusive),
            ("EXCLUSIVE", LockMode::Exclusive),
            ("ACCESS EXCLUSIVE", LockMode::AccessExclusive),
        ];
        for (text, mode) in modes {
            for table_kw in ["", "TABLE "] {
                for nowait in ["", " NOWAIT"] {
                    let sql = format!("LOCK {table_kw}t IN {text} MODE{nowait}");
                    let v = parse(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
                    assert_eq!(v.mode, mode, "{sql}");
                    assert_eq!(v.nowait, !nowait.is_empty(), "{sql}");
                    assert_eq!(v.table_names, vec!["t".to_string()]);
                }
            }
        }
    }

    #[test]
    fn default_mode_only_star_multi_and_dedup() {
        let v = parse("lock table only a *, b, a;").expect("ok");
        assert_eq!(v.mode, LockMode::AccessExclusive);
        assert!(!v.nowait);
        assert_eq!(v.table_names, vec!["a".to_string(), "b".to_string()]);
        let v = parse("LOCK a, ONLY b NOWAIT").expect("ok");
        assert!(v.nowait);
        assert_eq!(v.table_names.len(), 2);
    }

    #[test]
    fn rejects_malformed_forms() {
        for sql in [
            "LOCK",
            "LOCK TABLE",
            "LOCK t IN",
            "LOCK t IN SHARE",
            "LOCK t IN BOGUS MODE",
            "LOCK t IN SHARE MODE junk",
            "LOCK t USING OPERATION_ID 'x'",
            "LOCK t, ",
            "LOCK t NOWAIT NOWAIT",
            "LOCK t IN MODE",
            "LOCK t IN ACCESS SHARE EXCLUSIVE ROW MODE",
        ] {
            assert!(
                matches!(parse(sql), Err(SqlSurfaceError::UnsupportedSyntax { .. })),
                "{sql}"
            );
        }
    }
}
