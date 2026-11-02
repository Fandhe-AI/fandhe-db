//! 明示トランザクションの名前付き savepoint スタックと巻き戻し可否の種別
//! （SQL-69・Issue #1942・#1943）。
//!
//! `sql::transaction::ActiveTxn` が 1 つ保持し、`SessionTransaction` の
//! `savepoint`／`release_savepoint`／`rollback_to_savepoint` が操作する
//! （`core.rs` の `ParsedSql::Transaction` 分岐から呼ばれる）。永続データ表の巻き戻しは
//! redb の ephemeral savepoint で行う（Issue #1943）。redb の savepoint は「writer が最初に
//! テーブルを開く前の状態」にしか戻せないため、各エントリに [`SavepointKind`] を持たせ、
//! 巻き戻せるものだけ `ROLLBACK TO` を成功させる（詳細は `docs/design/explicit-transaction.md`）。
//! Tx 内の書き込み記録は [`WriteMarks`] で保存・復元する（Issue #1945）。
//! `operation_id` の Tx 内登録は水位で巻き戻す（Issue #1944。永続の台帳は redb の復元で戻る）。
//! キャッシュ等の復元は親 Issue #1941 の残りの子 Issue の範囲。
//!
//! 規則は PostgreSQL に準拠する。`SAVEPOINT` は常に push し、同名があっても古い側を
//! 残したまま新しい側が隠す。探索は新しい側から行い、`RELEASE` は見つけた savepoint と
//! それより新しいものをすべて pop する。名前の照合は大文字小文字を区別する完全一致
//! （lexer が引用の有無を保持しないため、混同を避ける安全側の選択）。
//! スタック長は `SessionTransaction` の文数上限で有界になる。
//!
//! 不変条件: redb の dirty フラグは一度立つと下りないため、スタックは古い側から
//! `[Restorable…, Unrestorable…]` の順に並ぶ。

use std::collections::{BTreeSet, HashSet};

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;

/// `SAVEPOINT` 時点の Tx 内書き込み記録（`ActiveTxn` の `has_writes`・`written_tables`・
/// `written_by_tenant`）の複製（Issue #1945）。`ROLLBACK TO` が redb の巻き戻しと対にして
/// 復元し、巻き戻した書き込みが読み取り経路の選択（`Writer` か確定スナップショットか）・
/// 遅延 FK の走査範囲・空 Tx の commit 判定に残らないようにする。`Restorable` の savepoint
/// だけが持つ（作成時点で redb 上 dirty でないため実質空の集合で、メモリ負担は小さい）。
#[derive(Debug, Clone, Default)]
pub(crate) struct WriteMarks {
    pub(crate) has_writes: bool,
    pub(crate) written_tables: HashSet<String>,
    pub(crate) written_by_tenant: BTreeSet<(String, String)>,
}

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
    /// `SAVEPOINT` 時点のセッション状態（`SET TIME ZONE`・`search_path`・セッション UDF 等）。
    /// `ROLLBACK TO` が redb の巻き戻しと対にして復元する（全体 `ROLLBACK` と同じ契約）。
    session: Box<SessionState>,
    /// `SAVEPOINT` 時点のカーソル識別子の高水位（`CursorRegistry::next_id`）。`ROLLBACK TO` は
    /// この値以上の識別子を持つカーソル（savepoint より後に宣言されたもの）を閉じる。
    cursor_watermark: u64,
    /// `SAVEPOINT` 時点の書き込み記録。`Restorable` のときだけ `Some`（`Unrestorable` は
    /// `ROLLBACK TO` 自体が拒否されるため保持しない）。
    write_marks: Option<WriteMarks>,
    /// `SAVEPOINT` 時点の `operation_id` 登録ログ長（Issue #1944）。`ROLLBACK TO` はこの位置より
    /// 後に登録された ID を取り消し、同じ Tx 内で再利用できるようにする。
    op_id_watermark: usize,
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
    /// 作成時点のセッション状態 `session` を複製して保持する。
    pub(crate) fn push(
        &mut self,
        name: &str,
        kind: SavepointKind,
        session: &SessionState,
        cursor_watermark: u64,
        op_id_watermark: usize,
        write_marks: Option<WriteMarks>,
    ) {
        self.entries.push(Entry {
            name: name.to_string(),
            kind,
            session: Box::new(session.clone()),
            cursor_watermark,
            op_id_watermark,
            write_marks,
        });
    }

    /// 位置 `pos` の savepoint 作成時点の書き込み記録。範囲外・`Unrestorable` は `None`。
    pub(crate) fn write_marks_at(&self, pos: usize) -> Option<&WriteMarks> {
        self.entries.get(pos).and_then(|e| e.write_marks.as_ref())
    }

    /// 位置 `pos` の savepoint 作成時点のセッション状態。範囲外は `None`。
    pub(crate) fn session_at(&self, pos: usize) -> Option<&SessionState> {
        self.entries.get(pos).map(|e| &*e.session)
    }

    /// 位置 `pos` の savepoint 作成時点のカーソル識別子の高水位。範囲外は `None`。
    pub(crate) fn cursor_watermark_at(&self, pos: usize) -> Option<u64> {
        self.entries.get(pos).map(|e| e.cursor_watermark)
    }

    /// 位置 `pos` の savepoint 作成時点の `operation_id` 登録ログ長。範囲外は `None`。
    pub(crate) fn op_id_watermark_at(&self, pos: usize) -> Option<usize> {
        self.entries.get(pos).map(|e| e.op_id_watermark)
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

    /// 最も新しい savepoint の位置（空なら `None`）。PL/pgSQL の内部 savepoint が積んだ直後に位置を
    /// 取るのに使う。
    pub(crate) fn top_position(&self) -> Option<usize> {
        self.entries.len().checked_sub(1)
    }

    /// 位置 `pos` 以上の savepoint をすべて取り除く（`pos` 自身を含む。範囲外は何もしない）。
    pub(crate) fn truncate_from(&mut self, pos: usize) {
        self.entries.truncate(pos);
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
    fn op_id_watermark_is_recorded_per_entry() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 1, None);
        s.push("b", R, &SessionState::default(), 0, 3, None);
        assert_eq!(s.op_id_watermark_at(0), Some(1));
        assert_eq!(s.op_id_watermark_at(1), Some(3));
        s.truncate_after(0);
        assert_eq!(s.op_id_watermark_at(1), None);
    }

    #[test]
    fn push_and_position_search_newest_first() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        s.push("b", R, &SessionState::default(), 0, 0, None);
        assert_eq!(s.len(), 2);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("b"), Some(1));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn same_name_shadows_older_one() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        s.push("a", R, &SessionState::default(), 0, 0, None);
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
        s.push("a", R, &SessionState::default(), 0, 0, None);
        s.push("b", R, &SessionState::default(), 0, 0, None);
        s.push("c", R, &SessionState::default(), 0, 0, None);
        s.release("b").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.position("a"), Some(0));
        assert_eq!(s.position("c"), None);
    }

    #[test]
    fn release_unknown_name_is_3b001_and_keeps_stack() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        let e = s.release("zzz").unwrap_err();
        assert_eq!(e.wire_code(), "3B001");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn names_match_case_sensitively() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        assert_eq!(s.position("A"), None);
        assert!(s.release("A").is_err());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn kind_is_kept_and_truncate_after_keeps_target() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        s.push("b", R, &SessionState::default(), 0, 0, None);
        s.push(
            "c",
            SavepointKind::Unrestorable,
            &SessionState::default(),
            0,
            0,
            None,
        );
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
        s.push(
            "a",
            SavepointKind::Unrestorable,
            &SessionState::default(),
            0,
            0,
            None,
        );
        assert!(!s.has_restorable());
        s.push("b", R, &SessionState::default(), 0, 0, None);
        assert!(s.has_restorable());
        s.release("b").unwrap();
        assert!(!s.has_restorable());
    }

    #[test]
    fn same_name_shadowing_uses_newest_kind() {
        let mut s = SavepointStack::new();
        s.push("a", R, &SessionState::default(), 0, 0, None);
        s.push(
            "a",
            SavepointKind::Unrestorable,
            &SessionState::default(),
            0,
            0,
            None,
        );
        let pos = s.position("a").unwrap();
        assert_eq!(s.kind_at(pos), Some(SavepointKind::Unrestorable));
    }

    #[test]
    fn write_marks_are_kept_per_entry_and_dropped_with_it() {
        let mut s = SavepointStack::new();
        let marks = WriteMarks {
            has_writes: true,
            written_tables: HashSet::from(["t".to_string()]),
            written_by_tenant: BTreeSet::from([("x".to_string(), "t".to_string())]),
        };
        s.push("a", R, &SessionState::default(), 0, 0, Some(marks));
        s.push(
            "b",
            R,
            &SessionState::default(),
            0,
            0,
            Some(WriteMarks::default()),
        );
        s.push(
            "c",
            SavepointKind::Unrestorable,
            &SessionState::default(),
            0,
            0,
            None,
        );
        assert!(s.write_marks_at(0).is_some_and(|m| m.has_writes));
        assert!(s.write_marks_at(1).is_some_and(|m| !m.has_writes));
        assert!(s.write_marks_at(2).is_none());
        assert!(s.write_marks_at(3).is_none());
        s.truncate_after(0);
        assert!(s.write_marks_at(1).is_none());
        s.release("a").unwrap();
        assert!(s.write_marks_at(0).is_none());
    }
}
