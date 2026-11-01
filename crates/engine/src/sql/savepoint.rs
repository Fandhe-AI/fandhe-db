//! 明示トランザクションの名前付き savepoint スタック（SQL-69・Issue #1942）。
//!
//! `sql::transaction::ActiveTxn` が 1 つ保持し、`SessionTransaction` の
//! `savepoint`／`release_savepoint`／`rollback_to_savepoint` が操作する
//! （`core.rs` の `ParsedSql::Transaction` 分岐から呼ばれる）。本 Issue の範囲は
//! 構文とスタックの管理までで、`ROLLBACK TO` の巻き戻し本体（台帳・キャッシュ・
//! カーソル等の復元）は親 Issue #1941 の範囲。
//!
//! 規則は PostgreSQL に準拠する。`SAVEPOINT` は常に push し、同名があっても古い側を
//! 残したまま新しい側が隠す。探索は新しい側から行い、`RELEASE` は見つけた savepoint と
//! それより新しいものをすべて pop する。名前の照合は大文字小文字を区別する完全一致
//! （lexer が引用の有無を保持しないため、混同を避ける安全側の選択）。
//! スタック長は `SessionTransaction` の文数上限で有界になる。

use crate::sql::allowlist::SqlSurfaceError;

/// savepoint 名の LIFO スタック（古い順に並ぶ）。
#[derive(Debug, Default)]
pub(crate) struct SavepointStack {
    entries: Vec<String>,
}

impl SavepointStack {
    /// 空のスタックを作る（`BEGIN` 時に `ActiveTxn` が初期化に使う）。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `SAVEPOINT name`。同名があっても重複して積む（新しい側が古い側を隠す）。
    pub(crate) fn push(&mut self, name: &str) {
        self.entries.push(name.to_string());
    }

    /// 最も新しい同名の位置（0 が最古）。無ければ `None`。
    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().rposition(|n| n == name)
    }

    /// `RELEASE name`。最も新しい同名とそれより新しい savepoint をすべて pop する。
    /// 見つからなければ `3B001` を返し、スタックは変えない。
    pub(crate) fn release(&mut self, name: &str) -> Result<(), SqlSurfaceError> {
        match self.position(name) {
            Some(pos) => {
                self.entries.truncate(pos);
                Ok(())
            }
            None => Err(SqlSurfaceError::InvalidSavepointSpecification),
        }
    }

    /// 現在積まれている savepoint 数（単体テストの検証用）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_position_search_newest_first() {
        let mut s = SavepointStack::new();
        s.push("a");
        s.push("b");
        assert_eq!(s.len(), 2);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("b"), Some(1));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn same_name_shadows_older_one() {
        let mut s = SavepointStack::new();
        s.push("a");
        s.push("a");
        assert_eq!(s.position("a"), Some(1));
        s.release("a").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.position("a"), Some(0));
        s.release("a").unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.release("a").is_err());
    }

    #[test]
    fn release_pops_newer_savepoints_too() {
        let mut s = SavepointStack::new();
        s.push("a");
        s.push("b");
        s.push("c");
        s.release("b").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn release_unknown_name_is_3b001_and_keeps_stack() {
        let mut s = SavepointStack::new();
        s.push("a");
        let e = s.release("zzz").unwrap_err();
        assert_eq!(e.wire_code(), "3B001");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn names_match_case_sensitively() {
        let mut s = SavepointStack::new();
        s.push("a");
        assert_eq!(s.position("A"), None);
        assert!(s.release("A").is_err());
        assert_eq!(s.len(), 1);
    }
}
