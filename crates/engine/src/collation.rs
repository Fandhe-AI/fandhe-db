//! 列・式に付ける照合順序（`COLLATE`）の許可リストとカタログ表現（TABLE-28、Issue #1609・#1610）。
//!
//! SQL 表層（`sql::allowlist`）が構文段で照合名を解決し、`catalog` が列行の `param`
//! フィールドへ保存・復元する際の共通語彙を提供する。
//!
//! Issue #1610 で、順序付けと索引のディスパッチ点（`ORDER BY`・`GROUP BY`・`DISTINCT` の
//! キー抽出、`MIN`/`MAX`(TEXT)、スカラー二次索引の構築）を [`Collation`] の網羅 `match`
//! （[`Collation::is_byte_order`]・[`Collation::compare`]・[`Collation::sort_key`]）経由に
//! 結線した。実効照合は「明示 `COLLATE` → 列の照合 → 既定 [`Collation::DEFAULT`]」の順
//! （[`Collation::effective`]）。結線していない経路（式の比較・JOIN・ウィンドウ・集合演算）は、
//! バイト列順でない照合が加わった時点で `sql` 層の `require_wired_collation` が `0A000` で
//! 拒否する（fail-closed）。等値だけの経路（`=`・DISTINCT/GROUP BY のグループ同一性・
//! UNIQUE/FK の正準キー・JOIN の等値キー・集合演算の重複除去）は変更しない。deterministic な
//! 照合では等値がバイト列の等値と一致する（[`Collation::compare`] の契約）ため、キーの符号化を
//! 照合依存にする必要がない。
//!
//! ICU 照合（`und-x-icu` 等）の比較器・照合の導出と衝突（`42P22`／`42P21`）は #2189・#2190、
//! 照合込みの索引は #2191 が担う。ICU の比較器が無い間に ICU 名を受理すると、宣言とは違う
//! バイト列順で黙って比較される列ができてしまう（意味論の fail-open）ため、受理するのは
//! 依存なしで実装できる `C` と `POSIX`（どちらもバイト列順）に限る。ICU 照合を追加するときは
//! [`Collation`] へ variant を足す（ワイルドカード腕を使っていないため、全ディスパッチ地点が
//! コンパイルエラーで列挙される）。方針は `docs/design/icu-collator-adoption.md` の決定事項 3
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
    /// 照合指定なし（`None`）が意味する既定照合（バイト列順）。
    pub const DEFAULT: Collation = Collation::C;

    /// 実効照合を決める（明示 `COLLATE` → 列の照合 → [`Self::DEFAULT`]）。照合の導出と衝突
    /// （`42P21`／`42P22`）は #2190 の担当で、ここは優先順位だけを持つ。
    pub fn effective(explicit: Option<Collation>, column: Option<Collation>) -> Collation {
        explicit.or(column).unwrap_or(Self::DEFAULT)
    }

    /// バイト列順と同一の順序を与える照合か。`false` の照合は、バイト列順を前提にした経路
    /// （索引の辞書・グルーピング用の索引走査）を使わず全走査へ縮退させること。
    pub fn is_byte_order(self) -> bool {
        match self {
            Collation::C | Collation::Posix => true,
        }
    }

    /// 2 つのテキストを照合で比較する全順序。契約: 照合が等しいと判定した値は、バイト列順で
    /// 二次比較して全順序にする（deterministic collation。`docs/design/icu-collator-adoption.md`
    /// §5）。`C`／`POSIX` はバイト列順そのもので二次比較は何もしない。
    pub fn compare(self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        match self {
            Collation::C | Collation::Posix => a.cmp(b),
        }
    }

    /// 照合順をバイト列の辞書順へ写すソートキー。`C`／`POSIX` は入力の借用でアロケーション
    /// しない。将来の ICU 照合は「照合キー ‖ 区切り ‖ 元バイト列」の形にし、キーの等値が
    /// 元バイト列の等値と一致するようにする（#2190）。
    pub fn sort_key(self, s: &[u8]) -> std::borrow::Cow<'_, [u8]> {
        match self {
            Collation::C | Collation::Posix => std::borrow::Cow::Borrowed(s),
        }
    }

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

    /// TABLE-28: 実効照合の優先順位と、バイト列順照合の比較・ソートキー。
    #[test]
    fn effective_compare_and_sort_key() {
        use std::cmp::Ordering;
        assert_eq!(Collation::effective(None, None), Collation::DEFAULT);
        assert_eq!(
            Collation::effective(None, Some(Collation::Posix)),
            Collation::Posix
        );
        assert_eq!(
            Collation::effective(Some(Collation::C), Some(Collation::Posix)),
            Collation::C
        );
        let vals: [&[u8]; 5] = [b"", b"B", b"a", b"ab", "é".as_bytes()];
        for c in [Collation::C, Collation::Posix] {
            assert!(c.is_byte_order());
            for a in vals {
                assert_eq!(c.compare(a, a), Ordering::Equal);
                assert!(matches!(c.sort_key(a), std::borrow::Cow::Borrowed(_)));
                for b in vals {
                    assert_eq!(c.compare(a, b), c.compare(b, a).reverse());
                    assert_eq!(c.compare(a, b), c.sort_key(a).cmp(&c.sort_key(b)));
                    assert_eq!(c.compare(a, b), a.cmp(b));
                }
            }
        }
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
