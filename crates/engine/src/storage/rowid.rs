//! 行ストアの物理キー要素「rowid」の型と、物理キー組み立て・分解の一点集約
//! （Issue #2193。ビヘイビア: TABLE-23。関連: TABLE-12。設計記録:
//! `docs/design/rowid-primary-key-separation.md`）。
//!
//! 責務境界: 行ストア（`rows` 表・`user_rows/{table}` 表）の物理キーの `u64` 要素を
//! 「物理 rowid」として型で区別する。契約の詳細は spec のポインタ（TABLE-23）を正とし、
//! 本コメントには記載しない。呼び出し文脈は `storage.rs`（`Storage::put` 等）・
//! `txn.rs`・`recovery/commit_boundary.rs`・`catalog.rs`・`tenant.rs` の行ストア操作で、
//! `id: u64` を受け取る公開 API は入口で [`RowId::from_legacy_id`] を通す。
//!
//! 非露出の契約: [`RowId`] は `Display` を実装しない。rowid の値を SQL 応答・EXPLAIN・
//! エラー文面・ログへ出さないための型レベルの防壁であり、`format!` へ直接渡せない
//! ことで混入を防ぐ。従来方式の表で値を出す必要がある箇所は [`RowId::legacy_id`] を
//! 明示的に通る（呼び出し箇所が後続 sub-issue の作業一覧になる）。

/// 行ストアの物理 rowid。
///
/// 呼び出し元: 行ストアを触る `engine` 内部のモジュール。外部クレートへは公開しない。
/// `Display` は意図的に実装しない（モジュール doc の非露出契約）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RowId(u64);

impl RowId {
    /// 従来方式の表で、利用者の `id` 列値から rowid を得る唯一の入口。
    pub(crate) const fn from_legacy_id(id: u64) -> Self {
        Self(id)
    }

    /// 従来方式の表で、rowid から `id` 列値を得る唯一の出口。
    /// 未移行モジュール（索引・検索・台帳）へ u64 を渡す境界でも使う。
    pub(crate) const fn legacy_id(self) -> u64 {
        self.0
    }

    /// 検索系 arena が保持する生値（行ストアの物理キー要素）から rowid を作る入口
    /// （Issue #2195。`arena::VectorArena` が行スキャンで受け取った値を格納する側と、
    /// 取り出す側 [`crate::arena::VectorArena::row_id`] の双方がここを通る）。
    pub(crate) const fn from_arena_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// arena の内部格納用に生値を取り出す（`arena.rs` の `push_row` 専用）。
    pub(crate) const fn raw_for_arena(self) -> u64 {
        self.0
    }

    /// 採番器（`catalog::rowid_alloc`）が払い出した値から rowid を作る。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn from_allocated(raw: u64) -> Self {
        Self(raw)
    }

    /// 採番器の状態保存用に生値を取り出す（`catalog::rowid_alloc` 専用）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn raw_for_state(self) -> u64 {
        self.0
    }
}

impl RowId {
    /// 索引（永続一意索引・FK 用キー索引）へ行参照として永続化する生値を得る。
    ///
    /// 不変条件: 索引の行参照のバイト列は、行ストア物理キーの `u64` 要素と同一
    /// （従来方式の表では利用者 `id` とも一致する）。永続フォーマットを変えないため、
    /// 索引コードは `legacy_id` / `raw_for_state` を経由せず本入口を使う（Issue #2194）。
    pub(crate) const fn to_index_raw(self) -> u64 {
        self.0
    }

    /// 索引に永続化された行参照の生値から rowid を復元する（[`RowId::to_index_raw`] の逆）。
    pub(crate) const fn from_index_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// 検索結果（公開型 `SearchHit.id` 等）へ詰める応答 `id` を rowid から得る**唯一の境界**
/// （Issue #2195。ビヘイビア: TABLE-23・SEARCH-1。設計記録: ADR D8）。
///
/// 現状は従来方式（`RowIdentity::LegacyId`）の表でしか到達しない（採番方式の表は
/// 読み取りゲートで拒否される）ため、`id` 列値＝rowid の生値を返す。採番方式のゲートを
/// 外す後続 sub-issue（#2197）は、本関数を「主キー列値を返す」実装へ差し替える。
/// 検索経路が rowid を応答へ直接詰める箇所をここ 1 箇所に集約する。
pub(crate) const fn hit_id_for_response(rowid: RowId) -> u64 {
    rowid.legacy_id()
}

/// 行ストアの物理キーを組み立てる唯一の生成点。
pub(crate) fn row_key(tenant: &str, rowid: RowId) -> (&str, u64) {
    (tenant, rowid.0)
}

/// 物理キーを `(tenant, RowId)` へ分解する（[`row_key`] の逆）。
pub(crate) fn split_row_key(key: (&str, u64)) -> (&str, RowId) {
    (key.0, RowId(key.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_key_roundtrip() {
        let r = RowId::from_legacy_id(42);
        let k = row_key("t", r);
        assert_eq!(k, ("t", 42));
        let (t, back) = split_row_key(k);
        assert_eq!(t, "t");
        assert_eq!(back, r);
        assert_eq!(back.legacy_id(), 42);
    }

    #[test]
    fn index_raw_matches_row_key_element() {
        let r = RowId::from_allocated(1234);
        assert_eq!(r.to_index_raw(), row_key("t", r).1);
        assert_eq!(RowId::from_index_raw(row_key("t", r).1), r);
    }

    #[test]
    fn arena_raw_roundtrip_and_response_boundary() {
        let r = RowId::from_arena_raw(7);
        assert_eq!(r.raw_for_arena(), 7);
        assert_eq!(hit_id_for_response(r), 7);
        assert_eq!(r, RowId::from_legacy_id(7));
    }

    #[test]
    fn ordering_matches_raw_key_order() {
        let a = RowId::from_legacy_id(1);
        let b = RowId::from_legacy_id(u64::MAX);
        assert!(a < b);
        assert!(row_key("t", a) < row_key("t", b));
    }
}
