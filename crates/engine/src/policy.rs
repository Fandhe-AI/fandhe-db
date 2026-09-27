//! RLS 相当のテナント境界・可視性判定を担うモジュール（TASK-124・対象ビヘイビア: CORE-2）。
//!
//! `core.rs` の [`crate::core::VectorCore`] 実装（検索・行取得）は、テナント一致判定と
//! 可視性ラベル評価を必ず [`PolicyContext::is_visible`] の単一照合パスで行う。
//! 呼び出し側が独自にテナント文字列を比較する経路を作らないことで、判定ロジックの
//! 分岐を 1 箇所に集約し fail-closed を維持する（TASK-133 以降の RLS ポリシー評価は
//! このパスを拡張する前提で設計している）。
//!
//! TASK-89（対象ビヘイビア: TABLE-9）の可視性判定はこの単一照合パスへ統合した。
//! 判定の詳細は [`PolicyContext::is_visible`] の実装・テストを参照（ポインタ:
//! TASK-89 / TABLE-9。spec 本文は転記しない）。テーブル単位の物理分離は本タスクの
//! スコープ外（[`crate::tenant`] のモジュールドキュメント参照）。

use crate::storage::Visibility;

/// [`PolicyContext::new`] の構築時検証で発生するエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// 空のテナント ID はテナント境界判定を曖昧にするため拒否する（fail-closed。
    /// `storage.rs::RowInput::tenant_id` の既存方針と整合）。
    EmptyTenantId,
    /// テナント ID のバイト長が [`crate::storage::MAX_TENANT_ID_LEN`] を超過した。
    /// storage 層（`encode_row`/`decode_row`）が受理する行の `tenant_id` 上限と
    /// 同一の定数をそのまま参照する（二重定義しない）。`PolicyContext` がこの上限を
    /// 超えるテナント ID を無制限に保持できてしまうと、`ctx.tenant_id()` を用いた
    /// 呼び出し元がそのまま `RowInput::tenant_id` へ渡した際に storage 層で初めて
    /// 拒否される、という契約の不一致が生じるため、構築時点で同じ上限を課す
    /// （codex P1・Issue #137 対応）。
    TenantIdTooLong { len: usize, max: u16 },
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::EmptyTenantId => write!(f, "policy context: tenant_id must not be empty"),
            PolicyError::TenantIdTooLong { len, max } => write!(
                f,
                "policy context: tenant_id length {len} exceeds limit {max}"
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

/// `tenant_id` の構築時検証（[`PolicyContext::new`]・[`PolicyContext::with_visibilities`]
/// の共通前段）。所有化（`String` 化）する前に借用 `&str` のまま検証することで、
/// 上限超過・空文字列の入力に対して不要なアロケーションを発生させない
/// （untrusted 入力を無制限に確保しない方針。coding-rust.md 準拠）。
fn validate_tenant_id(tenant_id: &str) -> Result<(), PolicyError> {
    if tenant_id.is_empty() {
        return Err(PolicyError::EmptyTenantId);
    }
    // `storage.rs` 側は `tenant_id.as_bytes().len()`（バイト長）で上限判定するため、
    // ここでも `str::len()`（バイト長。文字数ではない）で揃える。
    let len = tenant_id.len();
    if len > crate::storage::MAX_TENANT_ID_LEN as usize {
        return Err(PolicyError::TenantIdTooLong {
            len,
            max: crate::storage::MAX_TENANT_ID_LEN,
        });
    }
    Ok(())
}

/// 呼び出し元（プロトコル層 → `core.rs`）が渡すアクセス主体のテナント境界・可視性文脈。
///
/// テナント ID と許可可視性ラベル集合を同居させ、判定 API を [`Self::is_visible`] に
/// 集約する（CORE-2: 独立のテナント層を作らない設計）。既定は最も狭い許可
/// （`Public` のみ）で、`Private` を見せるには構築時に明示付与する。
///
/// `PartialEq`/`Eq` は `rls.rs::PrefilterIndex::search` が構築時 ctx と検索時 ctx の
/// 完全一致（テナント ID・許可可視性集合の両方）を fail-closed に照合するために持つ
/// （TASK-133・別テナント／可視性が狭化・拡大された ctx でのインデックス転用を検出する
/// ための同値判定。security.md P0「テナント分離の検査を外す/緩める/バイパス経路を
/// 作らない」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    tenant_id: String,
    allowed: AllowedVisibilities,
}

/// [`PolicyContext`] 内部でのみ使う許可可視性フラグ（Issue #354・呼び出しコスト軽量化。
/// 契約不変）。
///
/// `Visibility` は 2 値（`Public`/`Private`）の閉じた集合であるため、[`std::collections::HashSet`]
/// による集合表現（`contains` の都度 `SipHash` を計算する）を、構築時に確定する 2 個の
/// `bool` フラグへ置き換える。`{Public, Private}` の部分集合と `(bool, bool)` は全単射なので、
/// 導出 `PartialEq`/`Eq` の意味論は元の `HashSet` 版の集合同値と厳密に一致し、
/// `rls.rs::PrefilterIndex::search` が構築時 ctx と検索時 ctx の完全一致照合に用いる
/// 契約（TASK-133）は変わらない。判定に使う値の集合が変わるだけで、[`PolicyContext::is_visible`]
/// の判定順序・チェック回数・fail-closed（空集合 → 両フラグ false → 常に不可視）は不変。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllowedVisibilities {
    public: bool,
    private: bool,
}

impl AllowedVisibilities {
    /// 許可可視性の反復から事前計算する（重複入力は冪等。`HashSet` 挿入と同値）。
    fn from_visibilities(visibilities: impl IntoIterator<Item = Visibility>) -> Self {
        let mut allowed = Self {
            public: false,
            private: false,
        };
        for v in visibilities {
            match v {
                Visibility::Public => allowed.public = true,
                Visibility::Private => allowed.private = true,
            }
        }
        allowed
    }

    /// 指定した可視性ラベルが許可集合に含まれるか（`HashSet::contains` 相当）。
    #[inline]
    fn contains(&self, visibility: Visibility) -> bool {
        match visibility {
            Visibility::Public => self.public,
            Visibility::Private => self.private,
        }
    }
}

impl PolicyContext {
    /// `Public` のみ可視の `PolicyContext` を構築する（既定・最小権限）。
    /// 空テナント ID・[`crate::storage::MAX_TENANT_ID_LEN`] 超過は `Err`（fail-closed）。
    pub fn new(tenant_id: &str) -> Result<Self, PolicyError> {
        Self::with_visibilities(tenant_id, [Visibility::Public])
    }

    /// 許可可視性ラベル集合を明示指定して構築する。`Private` を見せる呼び出し元は
    /// ここへ明示的に `Visibility::Private` を含める必要がある（黙示の昇格を許さない）。
    ///
    /// `tenant_id` は借用 `&str` で受け取り、[`validate_tenant_id`] で検証してから
    /// `String` へ所有化する（`impl Into<String>` で先に所有化してから検証すると、
    /// 上限超過・不正な入力に対しても無条件にアロケーションが発生してしまうため。
    /// codex P1・Issue #137 対応）。
    pub fn with_visibilities(
        tenant_id: &str,
        visibilities: impl IntoIterator<Item = Visibility>,
    ) -> Result<Self, PolicyError> {
        validate_tenant_id(tenant_id)?;
        Ok(Self {
            tenant_id: tenant_id.to_string(),
            allowed: AllowedVisibilities::from_visibilities(visibilities),
        })
    }

    /// このコンテキストが属するテナント ID。
    #[inline]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// テナント一致判定と可視性ラベル評価を単一の照合パスで行う（CORE-2・
    /// TASK-89・対象ビヘイビア: TABLE-9）。判定条件は下記実装本体と
    /// このファイルのテストを参照（ポインタ: TABLE-9。挙動の詳細は spec 本文を
    /// 転記しないためここでは記述しない）。
    ///
    /// 呼び出し側（検索カーネル・行取得）はこのメソッド以外でテナント比較を
    /// 行わない。
    ///
    /// ホットパス（全読み取り経路で毎行・defense-in-depth により最大 2 回呼ばれる。
    /// Issue #354）のため `#[inline]` を付与する。判定順序（許可可視性集合 → `Public`
    /// 短絡 → テナント比較）・チェック回数・fail-closed は変更しない。
    #[inline]
    pub fn is_visible(&self, row_tenant: &str, row_visibility: Visibility) -> bool {
        if !self.allowed.contains(row_visibility) {
            return false;
        }
        // バイト列の一発比較（memcmp 相当）に確実に落とすため `as_bytes()` で比較する
        // （Issue #354）。`tenant_id` は `String` のまま保持し `tenant_id()` API・
        // 構築時検証 `validate_tenant_id` は不変。
        row_visibility == Visibility::Public || row_tenant.as_bytes() == self.tenant_id.as_bytes()
    }

    /// 書き込み認可の単一照合パス（TASK-95・対象ビヘイビア: RECOVER-4）。
    ///
    /// [`Self::is_visible`] とは独立の判定で、可視性ラベル（`Public`/`Private`）は
    /// 一切考慮しない。テナント一致のみで判定するため、他テナントの `Public` 行は
    /// 読めても書けない（読み取り可視性の拡張が書き込み権限の拡張を意味しない）。
    /// 呼び出し元（[`crate::tenant`]・[`crate::core`]）はこのメソッド以外で書き込み用の
    /// テナント比較を行わない（security.md P0「テナント分離の検査を外す/緩める/
    /// バイパス経路を作らない」）。
    #[inline]
    pub fn is_owner(&self, row_tenant: &str) -> bool {
        row_tenant.as_bytes() == self.tenant_id.as_bytes()
    }

    /// このコンテキストが他テナントの `Public` 行を可視とするか（Issue #915・
    /// SQL-25）。`true`（既定の [`Self::new`] を含む多くのセッション）だと
    /// `id` 順の並べ替えでも自テナントのパーティション走査だけでは可視行を
    /// 網羅できない（`sql::scan` の経路 (A)／(B) の分岐に使う）。判定ロジック
    /// 自体は増やさず、[`Self::is_visible`] が既に参照する `allowed.public`
    /// を読み取り専用で公開するのみ（security.md P0: 判定パスを増やさない）。
    #[inline]
    pub(crate) fn allows_public(&self) -> bool {
        self.allowed.public
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 対象ビヘイビア: CORE-2。同一テナント・Public は可視。
    #[test]
    fn same_tenant_public_is_visible() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(ctx.is_visible("tenant-a", Visibility::Public));
    }

    // 対象ビヘイビア: TABLE-9。
    #[test]
    fn other_tenant_public_row_is_visible_when_public_is_allowed() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(ctx.is_visible("tenant-b", Visibility::Public));
        assert!(!ctx.is_visible("tenant-b", Visibility::Private));
    }

    // 対象ビヘイビア: TABLE-9（fail-closed）。
    #[test]
    fn other_tenant_public_row_is_not_visible_without_public_grant() {
        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        assert!(!ctx.is_visible("tenant-b", Visibility::Public));
    }

    // 対象ビヘイビア: CORE-2。Private は明示付与なしでは不可視（既定は最小権限）。
    #[test]
    fn private_requires_explicit_grant() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(!ctx.is_visible("tenant-a", Visibility::Private));

        let ctx_with_private = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
            .expect("valid tenant");
        assert!(ctx_with_private.is_visible("tenant-a", Visibility::Private));
    }

    // 対象ビヘイビア: CORE-2。空テナント ID は構築時に拒否する（fail-closed）。
    #[test]
    fn empty_tenant_id_is_rejected() {
        assert_eq!(
            PolicyContext::new("").unwrap_err(),
            PolicyError::EmptyTenantId
        );
    }

    // レビュー指摘対応（codex P1・Issue #137）: `storage.rs::MAX_TENANT_ID_LEN` ちょうどの
    // 長さのテナント ID は構築を許可する（境界値の pass 側）。
    #[test]
    fn tenant_id_at_max_len_is_accepted() {
        let max_len = crate::storage::MAX_TENANT_ID_LEN as usize;
        let tenant_id = "t".repeat(max_len);
        let ctx = PolicyContext::new(&tenant_id).expect("tenant_id at the limit must be accepted");
        assert_eq!(ctx.tenant_id(), tenant_id);
    }

    // 対象ビヘイビア: RECOVER-4。同一テナントの行は所有者とみなす。
    #[test]
    fn is_owner_true_for_same_tenant() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(ctx.is_owner("tenant-a"));
    }

    // 対象ビヘイビア: RECOVER-4（fail-closed）。他テナントの行は所有者とみなさない。
    #[test]
    fn is_owner_false_for_other_tenant() {
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        assert!(!ctx.is_owner("tenant-b"));
    }

    // 対象ビヘイビア: RECOVER-4。`is_owner` は可視性ラベルを一切考慮しない。他テナントの
    // `Public` 行が読めても（`is_visible`）、書き込み認可（`is_owner`）は別判定であることを
    // 確認する（読み取り可視性の拡張が書き込み権限の拡張を意味しないことの回帰検証）。
    #[test]
    fn is_owner_ignores_visibility_even_when_public_is_visible() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert!(ctx.is_visible("tenant-b", Visibility::Public));
        assert!(!ctx.is_owner("tenant-b"));
    }

    // レビュー指摘対応（codex P1・Issue #137）: `storage.rs::MAX_TENANT_ID_LEN` を 1 バイト
    // でも超えるテナント ID は構築時に拒否する（境界値の reject 側。fail-closed）。
    // storage 層（`encode_row`）が同じ上限で拒否する行と契約を揃える。
    #[test]
    fn tenant_id_over_max_len_is_rejected() {
        let max_len = crate::storage::MAX_TENANT_ID_LEN as usize;
        let tenant_id = "t".repeat(max_len + 1);
        assert_eq!(
            PolicyContext::new(&tenant_id).unwrap_err(),
            PolicyError::TenantIdTooLong {
                len: max_len + 1,
                max: crate::storage::MAX_TENANT_ID_LEN,
            }
        );
    }

    // Issue #354（`AllowedVisibilities` フラグ表現への置換）の回帰: 空の可視性反復から
    // 構築した ctx はいかなる行も不可視（fail-closed 維持）。
    #[test]
    fn empty_visibilities_iterator_makes_everything_invisible() {
        let ctx =
            PolicyContext::with_visibilities("tenant-a", std::iter::empty()).expect("valid tenant");
        assert!(!ctx.is_visible("tenant-a", Visibility::Public));
        assert!(!ctx.is_visible("tenant-a", Visibility::Private));
    }

    // Issue #354: 重複を含む可視性反復は冪等（`HashSet` 挿入と同じ意味論）。
    // 判定結果・ctx の同値性のいずれも重複の有無で変わらない。
    #[test]
    fn duplicate_visibilities_are_idempotent() {
        let dup = PolicyContext::with_visibilities(
            "tenant-a",
            [Visibility::Public, Visibility::Public, Visibility::Private],
        )
        .expect("valid tenant");
        let once =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        assert_eq!(dup, once);
        assert!(dup.is_visible("tenant-a", Visibility::Public));
        assert!(dup.is_visible("tenant-a", Visibility::Private));
    }

    // Issue #354: 入力順序が異なっても構築される ctx は同値（集合同値の意味論維持。
    // `rls.rs::PrefilterIndex::search` の ctx 完全一致照合が入力順序に依存しないことの回帰）。
    #[test]
    fn visibility_order_does_not_affect_equality() {
        let a =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let b =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Private, Visibility::Public])
                .expect("valid tenant");
        assert_eq!(a, b);
    }
}
