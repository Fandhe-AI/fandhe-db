//! 明示トランザクションの名前付き savepoint スタックと巻き戻し可否の種別
//! （SQL-69・Issue #1942・#1943）。
//!
//! `sql::transaction::ActiveTxn` が 1 つ保持し、`SessionTransaction` の
//! `savepoint`／`release_savepoint`／`rollback_to_savepoint` が操作する
//! （`core.rs` の `ParsedSql::Transaction` 分岐から呼ばれる）。永続データ表の巻き戻しは
//! redb の ephemeral savepoint で行う（Issue #1943）。redb の savepoint は「writer が最初に
//! テーブルを開く前の状態」にしか戻せないため、各エントリに [`SavepointKind`] を持たせ、
//! 巻き戻せるものだけ `ROLLBACK TO` を成功させる（詳細は `docs/design/explicit-transaction.md`）。
//! 台帳・キャッシュ・カーソル等の復元は親 Issue #1941 の残りの子 Issue の範囲。
//!
//! 規則は PostgreSQL に準拠する。`SAVEPOINT` は常に push し、同名があっても古い側を
//! 残したまま新しい側が隠す。探索は新しい側から行い、`RELEASE` は見つけた savepoint と
//! それより新しいものをすべて pop する。名前の照合は大文字小文字を区別する完全一致
//! （lexer が引用の有無を保持しないため、混同を避ける安全側の選択）。
//! スタック長は `SessionTransaction` の文数上限で有界になる。
//!
//! 不変条件: redb の dirty フラグは一度立つと下りないため、スタックは古い側から
//! `[Restorable…, Unrestorable…]` の順に並ぶ。

use crate::sql::allowlist::SqlSurfaceError;

/// savepoint の巻き戻し可否（redb の ephemeral savepoint の制約による）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavepointKind {
    /// 作成時に writer が未取得、または redb 上で dirty でなかった。基底 savepoint へ戻せる。
    Restorable,
    /// 作成時に writer が既に書き込み済み（dirty）だった。redb の API では戻せず、
    /// `ROLLBACK TO` は `0A000` で fail-closed に拒否する。
    Unrestorable,
}

#[derive(Debug)]
struct Entry {
    name: String,
    kind: SavepointKind,
}

/// savepoint 名の LIFO スタック（古い順に並ぶ）。
#[derive(Debug, Default)]
pub(crate) struct SavepointStack {
    entries: Vec<Entry>,
}

impl SavepointStack {
    /// 空のスタックを作る（`BEGIN` 時に `ActiveTxn` が初期化に使う）。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `SAVEPOINT name`。同名があっても重複して積む（新しい側が古い側を隠す）。
    pub(crate) fn push(&mut self, name: &str, kind: SavepointKind) {
        self.entries.push(Entry {
            name: name.to_string(),
            kind,
        });
    }

    /// 最も新しい同名の位置（0 が最古）。無ければ `None`。
    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.entries.iter().rposition(|e| e.name == name)
    }

    /// 位置 `pos` の savepoint の種別。範囲外は `None`。
    pub(crate) fn kind_at(&self, pos: usize) -> Option<SavepointKind> {
        self.entries.get(pos).map(|e| e.kind)
    }

    /// `ROLLBACK TO` の成功後に、位置 `pos` より新しい savepoint を取り除く（`pos` 自身は残す）。
    pub(crate) fn truncate_after(&mut self, pos: usize) {
        self.entries.truncate(pos.saturating_add(1));
    }

    /// 巻き戻せる savepoint が 1 つでも積まれているか（基底 savepoint の要否判定）。
    pub(crate) fn has_restorable(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.kind == SavepointKind::Restorable)
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

    const R: SavepointKind = SavepointKind::Restorable;

    #[test]
    fn push_and_position_search_newest_first() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        s.push("b", R);
        assert_eq!(s.len(), 2);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("b"), Some(1));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn same_name_shadows_older_one() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        s.push("a", R);
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
        s.push("a", R);
        s.push("b", R);
        s.push("c", R);
        s.release("b").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn release_unknown_name_is_3b001_and_keeps_stack() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        let e = s.release("zzz").unwrap_err();
        assert_eq!(e.wire_code(), "3B001");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn names_match_case_sensitively() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        assert_eq!(s.position("A"), None);
        assert!(s.release("A").is_err());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn kind_is_kept_and_truncate_after_keeps_target() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        s.push("b", R);
        s.push("c", SavepointKind::Unrestorable);
        assert_eq!(s.kind_at(0), Some(R));
        assert_eq!(s.kind_at(2), Some(SavepointKind::Unrestorable));
        assert_eq!(s.kind_at(3), None);
        s.truncate_after(1);
        assert_eq!(s.len(), 2);
        assert_eq!(s.position("b"), Some(1));
        assert_eq!(s.position("c"), None);
        s.truncate_after(0);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn has_restorable_tracks_kinds() {
        let mut s = SavepointStack::new();
        assert!(!s.has_restorable());
        s.push("a", SavepointKind::Unrestorable);
        assert!(!s.has_restorable());
        s.push("b", R);
        assert!(s.has_restorable());
        s.release("b").unwrap();
        assert!(!s.has_restorable());
    }

    #[test]
    fn same_name_shadowing_uses_newest_kind() {
        let mut s = SavepointStack::new();
        s.push("a", R);
        s.push("a", SavepointKind::Unrestorable);
        let pos = s.position("a").unwrap();
        assert_eq!(s.kind_at(pos), Some(SavepointKind::Unrestorable));
    }
}
