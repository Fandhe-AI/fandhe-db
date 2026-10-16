//! 列・式に付ける照合順序（`COLLATE`）の許可リストとカタログ表現（TABLE-28、Issue #1609）。
//!
//! SQL 表層（`sql::allowlist`）が構文段で照合名を解決し、`catalog` が列行の `param`
//! フィールドへ保存・復元する際の共通語彙を提供する。本 Issue の範囲は「解析と保存」
//! だけで、比較・ORDER BY・索引キーの照合対応は #1610、ICU 照合（`und-x-icu` 等）の
//! 比較器・照合の導出と衝突（`42P22`／`42P21`）は #2189・#2190、照合込みの索引は #2191
//! が担う。ICU の比較器が無い間に ICU 名を受理すると、宣言とは違うバイト列順で黙って比較される
//! 列ができてしまう（意味論の fail-open）ため、受理するのは依存なしで実装できる `C` と
//! `POSIX`（どちらもバイト列順）に限る。ICU 照合を追加するときは [`Collation`] へ
//! variant を足す（ワイルドカード腕を使っていないため、全ディスパッチ地点がコンパイル
//! エラーで列挙される）。方針は `docs/design/icu-collator-adoption.md` の決定事項 3
//! （固定の許可リストで解決し、ライブラリの構築成否を拒否の根拠にしない）に従う。

use crate::catalog::ColumnType;

/// 受理する照合（固定の許可リスト）。`Option<Collation>` の `None` は「照合指定なし」で、
/// 明示した `C`／`POSIX` とは区別して保存する（導出・衝突の判定が #2190 で必要とするため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Collation {
    /// `C`（バイト列順・決定的）。
    C,
    /// `POSIX`（`C` と同義だが別名として区別して保存する）。
    Posix,
}

impl Collation {
    /// SQL 上の照合名を許可リストと完全一致で解決する。未知の名前は `None`（呼び出し元が
    /// `42704` にする）。大小文字は畳まない（`"C"`／`"POSIX"` のみ）。固定比較だけで
    /// アロケーションせず、カタログも参照しないため、権限ゲートより前に呼んでもテーブルの
    /// 存在オラクルにならない。
    pub fn resolve_name(name: &str) -> Option<Self> {
        match name {
            "C" => Some(Collation::C),
            "POSIX" => Some(Collation::Posix),
            _ => None,
        }
    }

    /// 描画用の SQL 名（`COLLATE "<名前>"` の形で出す）。
    pub fn sql_name(self) -> &'static str {
        match self {
            Collation::C => "C",
            Collation::Posix => "POSIX",
        }
    }

    /// カタログ `param` 用のトークン。`[a-z0-9_]` の範囲に収める。ICU 名を追加する
    /// ときは `-` を `_` に置き換えた全単射のトークン（例: `und_x_icu`）を使うこと。
    pub(crate) fn catalog_token(self) -> &'static str {
        match self {
            Collation::C => "c",
            Collation::Posix => "posix",
        }
    }

    /// [`Self::catalog_token`] の逆変換。未知のトークンは `None`（decode 側が fail-closed）。
    pub(crate) fn from_catalog_token(token: &str) -> Option<Self> {
        match token {
            "c" => Some(Collation::C),
            "posix" => Some(Collation::Posix),
            _ => None,
        }
    }
}

/// 照合を付けられる型（`TEXT`・`VARCHAR[(n)]`・`CHAR(n)`）か。それ以外のスカラー型への
/// `COLLATE` は `42804`。網羅 match にして、型を追加したときに判断を強制する。
pub fn is_collatable(ty: &ColumnType) -> bool {
    match ty {
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => true,
        ColumnType::Vector(_)
        | ColumnType::SmallInt
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Time
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Enum(_)
        | ColumnType::Numeric { .. }
        | ColumnType::Uuid => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TABLE-28: 許可リストは完全一致のみ。
    #[test]
    fn resolve_name_is_exact_allowlist() {
        assert_eq!(Collation::resolve_name("C"), Some(Collation::C));
        assert_eq!(Collation::resolve_name("POSIX"), Some(Collation::Posix));
        for bad in ["c", "posix", "", "und-x-icu", "de_DE", "C ", "default"] {
            assert_eq!(Collation::resolve_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn catalog_token_round_trips_and_is_param_safe() {
        for c in [Collation::C, Collation::Posix] {
            let t = c.catalog_token();
            assert!(t
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'));
            assert_eq!(Collation::from_catalog_token(t), Some(c));
            assert_eq!(Collation::resolve_name(c.sql_name()), Some(c));
        }
        assert_eq!(Collation::from_catalog_token("C"), None);
        assert_eq!(Collation::from_catalog_token("icu"), None);
    }

    #[test]
    fn collatable_types() {
        assert!(is_collatable(&ColumnType::Text));
        assert!(is_collatable(&ColumnType::Varchar(None)));
        assert!(is_collatable(&ColumnType::Varchar(Some(3))));
        assert!(is_collatable(&ColumnType::Bpchar(3)));
        assert!(!is_collatable(&ColumnType::Integer));
        assert!(!is_collatable(&ColumnType::Vector(3)));
    }
}
