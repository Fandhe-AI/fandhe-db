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
//! ICU 照合（`und-x-icu`・`ja-x-icu`）は Issue #2190 で追加した。比較は ICU4X（`icu_collator`、
//! `compiled_data` のため実行時 I/O なし）の比較器で行い、等しいと判定した値はバイト列順で
//! 二次比較して全順序にする（deterministic collation）。ICU 名の解決は固定の許可リストとの
//! 完全一致のみで、ライブラリの構築成否を受理の根拠にしない（決定事項 3）。ICU の照合が
//! 結線されていない経路へ入った場合は `sql` 層の `require_wired_collation` が `0A000` で
//! 拒否する（意味論の fail-open を作らない）。式の照合の導出と衝突（`42P21`／`42P22`）は
//! `sql::udf_call` が担い、照合込みの索引は #2191 が担う。
//!
//! 新しい照合を足すときは [`Collation`] へ variant を足す（ワイルドカード腕を使っていない
//! ため、全ディスパッチ地点がコンパイルエラーで列挙される）。

use std::borrow::Cow;
use std::sync::LazyLock;

use crate::catalog::ColumnType;
use crate::sql::allowlist::SqlSurfaceError;

/// ICU 照合の比較器（`compiled_data` のため構築に I/O を伴わない）。構築失敗は `None` で
/// 持ち、[`Collation::ensure_available`] が束縛段で `XX000` にする。
type IcuCollator = icu_collator::CollatorBorrowed<'static>;

static UND_COLLATOR: LazyLock<Option<IcuCollator>> = LazyLock::new(|| {
    icu_collator::Collator::try_new(
        icu_collator::CollatorPreferences::default(),
        icu_collator::options::CollatorOptions::default(),
    )
    .ok()
});

static JA_COLLATOR: LazyLock<Option<IcuCollator>> = LazyLock::new(|| {
    // ロケール型は `from_locale_strict` の引数型から推論する（`icu_locale_core` を直接依存に
    // せず、依存を増やさない。docs/design/icu-collator-adoption.md の決定事項 5）。
    let prefs = icu_collator::CollatorPreferences::from_locale_strict(&"ja".parse().ok()?).ok()?;
    icu_collator::Collator::try_new(prefs, icu_collator::options::CollatorOptions::default()).ok()
});

/// ソートキーの長さ上限の係数（入力 1 バイトあたり）。ICU のキーは 1 コードポイントあたり
/// 数バイト×複数レベルで、展開の大きい文字でも 1 バイトあたり数十バイトに収まる。定数倍で
/// 頭打ちにして異常な膨張を fail-closed にする（DoS 対策）。
const SORT_KEY_BYTES_PER_INPUT_BYTE: usize = 64;
/// ソートキーの長さ上限の定数項（空文字・短い入力のレベル区切り分）。
const SORT_KEY_BASE_BYTES: usize = 64;
/// ソートキーを生成する入力の長さ上限（バイト）。TEXT の長さ上限より十分大きい。
const SORT_KEY_MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// 初期確保の入力 1 バイトあたりの見込み（超過分は書き込みで伸長する）。
const SORT_KEY_RESERVE_BYTES_PER_INPUT_BYTE: usize = 4;

/// 受理する照合（固定の許可リスト）。`Option<Collation>` の `None` は「照合指定なし」で、
/// 明示した `C`／`POSIX` とは区別して保存する（導出・衝突の判定が必要とするため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Collation {
    /// `C`（バイト列順・決定的）。
    C,
    /// `POSIX`（`C` と同義だが別名として区別して保存する）。
    Posix,
    /// `und-x-icu`（ICU の root 照合・既定オプション。Issue #2190）。
    UndIcu,
    /// `ja-x-icu`（ICU の日本語ロケール照合・既定オプション。Issue #2190）。
    JaIcu,
}

impl Collation {
    /// 照合指定なし（`None`）が意味する既定照合（バイト列順）。
    pub const DEFAULT: Collation = Collation::C;

    /// 実効照合を決める（明示 `COLLATE` → 列の照合 → [`Self::DEFAULT`]）。照合の導出と衝突
    /// （`42P21`／`42P22`）は `sql::udf_call` の導出が担い、ここは優先順位だけを持つ。
    pub fn effective(explicit: Option<Collation>, column: Option<Collation>) -> Collation {
        explicit.or(column).unwrap_or(Self::DEFAULT)
    }

    /// バイト列順と同一の順序を与える照合か。`false` の照合は、バイト列順を前提にした経路
    /// （索引の辞書・グルーピング用の索引走査）を使わず全走査へ縮退させること。
    pub fn is_byte_order(self) -> bool {
        match self {
            Collation::C | Collation::Posix => true,
            Collation::UndIcu | Collation::JaIcu => false,
        }
    }

    /// ICU の比較器（バイト列順の照合は `None`）。
    fn icu(self) -> Option<&'static IcuCollator> {
        match self {
            Collation::C | Collation::Posix => None,
            Collation::UndIcu => UND_COLLATOR.as_ref(),
            Collation::JaIcu => JA_COLLATOR.as_ref(),
        }
    }

    /// ICU 照合の比較器を構築できることを保証する。バイト列順でない照合が束縛済みの
    /// キー・比較ノードへ入る束縛地点で呼び、構築失敗を実行時ではなく束縛時に `XX000` で
    /// fail-closed にする（[`Self::compare`] は失敗しないシグネチャのため）。
    pub fn ensure_available(self) -> Result<(), SqlSurfaceError> {
        if self.is_byte_order() || self.icu().is_some() {
            Ok(())
        } else {
            Err(SqlSurfaceError::Internal {
                detail: "collation is unavailable".to_string(),
            })
        }
    }

    /// 2 つのテキストを照合で比較する全順序。契約: 照合が等しいと判定した値は、バイト列順で
    /// 二次比較して全順序にする（deterministic collation。`docs/design/icu-collator-adoption.md`
    /// §5）。`C`／`POSIX` はバイト列順そのもので二次比較は何もしない。ICU 照合の比較器が
    /// 使えない場合（[`Self::ensure_available`] を通した後は到達しない）はバイト列順に倒す
    /// （全順序を保ち、`Equal` に潰さない）。
    pub fn compare(self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        match self.icu() {
            None => {
                debug_assert!(self.is_byte_order(), "ICU collator must be available");
                a.cmp(b)
            }
            Some(c) => c.compare_utf8(a, b).then_with(|| a.cmp(b)),
        }
    }

    /// 照合順をバイト列の辞書順へ写すソートキー。`C`／`POSIX` は入力の借用でアロケーション
    /// しない。ICU 照合は「ICU キー ‖ `0x00` ‖ 元バイト列」の形にする。ICU キーは `0x00` を
    /// 含まない（レベル区切りは `0x01`／`0x02`）ため、キーが異なればキーで、等しければ
    /// 元バイト列で順序が決まり、[`Self::compare`] と一致する。キー生成は比較より高コスト
    /// なので、何度も比較されるキー（索引など。#2191）にだけ使う。長さは入力長に比例する
    /// 上限で拒否する。
    pub fn sort_key(self, s: &[u8]) -> Result<Cow<'_, [u8]>, SqlSurfaceError> {
        let Some(c) = self.icu() else {
            debug_assert!(self.is_byte_order(), "ICU collator must be available");
            return Ok(Cow::Borrowed(s));
        };
        let too_large = || SqlSurfaceError::payload_too_large("collation sort key is too large");
        if s.len() > SORT_KEY_MAX_INPUT_BYTES {
            return Err(too_large());
        }
        // `icu_collator::CollationKeySink` は `unstable` feature 配下で名前を出せないため
        // （feature の追加は依存変更になる）、標準実装の `Vec<u8>` を使い、初期確保を
        // `try_reserve` で行い、生成後に長さ上限を検査する。キー長は入力長の定数倍で
        // 抑えられる（上限は fail-closed の保険）。
        let limit = s
            .len()
            .checked_mul(SORT_KEY_BYTES_PER_INPUT_BYTE)
            .and_then(|n| n.checked_add(SORT_KEY_BASE_BYTES))
            .ok_or_else(too_large)?;
        let mut key: Vec<u8> = Vec::new();
        let reserve = s
            .len()
            .saturating_mul(SORT_KEY_RESERVE_BYTES_PER_INPUT_BYTE)
            .saturating_add(SORT_KEY_BASE_BYTES);
        key.try_reserve(reserve).map_err(|_| too_large())?;
        match c.write_sort_key_utf8_to(s, &mut key) {
            Ok(()) => {}
            Err(infallible) => match infallible {},
        }
        if key.len() > limit {
            return Err(too_large());
        }
        key.try_reserve_exact(s.len().saturating_add(1))
            .map_err(|_| too_large())?;
        key.push(0);
        key.extend_from_slice(s);
        Ok(Cow::Owned(key))
    }

    /// SQL 上の照合名を許可リストと完全一致で解決する。未知の名前は `None`（呼び出し元が
    /// `42704` にする）。大小文字は畳まない。固定比較だけでアロケーションせず、カタログも
    /// 参照しないため、権限ゲートより前に呼んでもテーブルの存在オラクルにならない。
    pub fn resolve_name(name: &str) -> Option<Self> {
        match name {
            "C" => Some(Collation::C),
            "POSIX" => Some(Collation::Posix),
            "und-x-icu" => Some(Collation::UndIcu),
            "ja-x-icu" => Some(Collation::JaIcu),
            _ => None,
        }
    }

    /// 描画用の SQL 名（`COLLATE "<名前>"` の形で出す）。
    pub fn sql_name(self) -> &'static str {
        match self {
            Collation::C => "C",
            Collation::Posix => "POSIX",
            Collation::UndIcu => "und-x-icu",
            Collation::JaIcu => "ja-x-icu",
        }
    }

    /// カタログ `param` 用のトークン。`[a-z0-9_]` の範囲に収める。ICU 名は `-` を `_` に
    /// 置き換えた全単射のトークン（`und_x_icu`）を使う。
    pub(crate) fn catalog_token(self) -> &'static str {
        match self {
            Collation::C => "c",
            Collation::Posix => "posix",
            Collation::UndIcu => "und_x_icu",
            Collation::JaIcu => "ja_x_icu",
        }
    }

    /// [`Self::catalog_token`] の逆変換。未知のトークンは `None`（decode 側が fail-closed）。
    pub(crate) fn from_catalog_token(token: &str) -> Option<Self> {
        match token {
            "c" => Some(Collation::C),
            "posix" => Some(Collation::Posix),
            "und_x_icu" => Some(Collation::UndIcu),
            "ja_x_icu" => Some(Collation::JaIcu),
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
    use std::cmp::Ordering;

    /// TABLE-28: 許可リストは完全一致のみ。
    #[test]
    fn resolve_name_is_exact_allowlist() {
        assert_eq!(Collation::resolve_name("C"), Some(Collation::C));
        assert_eq!(Collation::resolve_name("POSIX"), Some(Collation::Posix));
        assert_eq!(
            Collation::resolve_name("und-x-icu"),
            Some(Collation::UndIcu)
        );
        assert_eq!(Collation::resolve_name("ja-x-icu"), Some(Collation::JaIcu));
        for bad in [
            "c",
            "posix",
            "",
            "de_DE",
            "C ",
            "default",
            "UND-X-ICU",
            "und-x-icu ",
            " und-x-icu",
            "en-x-icu",
            "und_x_icu",
            "unicode",
            "ja-x-icu\0",
        ] {
            assert_eq!(Collation::resolve_name(bad), None, "{bad:?}");
        }
    }

    const ALL: [Collation; 4] = [
        Collation::C,
        Collation::Posix,
        Collation::UndIcu,
        Collation::JaIcu,
    ];

    #[test]
    fn catalog_token_round_trips_and_is_param_safe() {
        for c in ALL {
            let t = c.catalog_token();
            assert!(t
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'));
            assert_eq!(Collation::from_catalog_token(t), Some(c));
            assert_eq!(Collation::resolve_name(c.sql_name()), Some(c));
        }
        assert_eq!(Collation::from_catalog_token("C"), None);
        assert_eq!(Collation::from_catalog_token("icu"), None);
        assert_eq!(Collation::from_catalog_token("und-x-icu"), None);
    }

    /// TABLE-28: 実効照合の優先順位と、バイト列順照合の比較・ソートキー。
    #[test]
    fn effective_compare_and_sort_key() {
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
                assert!(matches!(c.sort_key(a), Ok(Cow::Borrowed(_))));
                for b in vals {
                    assert_eq!(c.compare(a, b), c.compare(b, a).reverse());
                    assert_eq!(
                        c.compare(a, b),
                        c.sort_key(a).unwrap().cmp(&c.sort_key(b).unwrap())
                    );
                    assert_eq!(c.compare(a, b), a.cmp(b));
                }
            }
        }
    }

    /// 性質テスト用の固定の値プール（ASCII の大小・数字・記号・アクセント・結合文字の
    /// NFC/NFD・かな/カナ・漢字・絵文字・空文字）。
    fn pool() -> Vec<&'static str> {
        vec![
            "",
            " ",
            "-",
            "_",
            "0",
            "1",
            "10",
            "2",
            "a",
            "A",
            "ab",
            "Ab",
            "aB",
            "AB",
            "b",
            "B",
            "z",
            "e",
            "E",
            "\u{e9}",
            "e\u{301}",
            "\u{c9}",
            "f",
            "\u{3042}",
            "\u{30a2}",
            "\u{3044}",
            "\u{30a4}",
            "\u{304b}",
            "\u{304c}",
            "\u{6f22}",
            "\u{5b57}",
            "\u{1f600}",
            "a b",
            "a-b",
            "ab ",
        ]
    }

    /// TABLE-28: ICU 照合の比較が全順序（反対称・推移・Equal はバイト等値）で、ソートキーの
    /// バイト比較と一致すること。ICU キーは 0x00 を含まない（区切りバイトの前提）。
    #[test]
    fn icu_compare_is_total_order_and_matches_sort_key() {
        let pool = pool();
        for c in [Collation::UndIcu, Collation::JaIcu] {
            assert!(!c.is_byte_order());
            assert!(c.ensure_available().is_ok());
            for a in &pool {
                let ka = c.sort_key(a.as_bytes()).unwrap();
                assert!(matches!(ka, Cow::Owned(_)));
                let split = ka.len() - a.len() - 1;
                assert!(
                    !ka[..split].contains(&0),
                    "ICU key must not contain 0x00: {a:?}"
                );
                assert_eq!(ka[split], 0);
                assert_eq!(&ka[split + 1..], a.as_bytes());
                for b in &pool {
                    let ab = c.compare(a.as_bytes(), b.as_bytes());
                    let ba = c.compare(b.as_bytes(), a.as_bytes());
                    assert_eq!(ab, ba.reverse(), "{a:?} {b:?}");
                    assert_eq!(ab == Ordering::Equal, a == b, "{a:?} {b:?}");
                    let kb = c.sort_key(b.as_bytes()).unwrap();
                    assert_eq!(ab, ka.cmp(&kb), "{c:?} {a:?} {b:?}");
                    for d in &pool {
                        if ab != Ordering::Greater
                            && c.compare(b.as_bytes(), d.as_bytes()) != Ordering::Greater
                        {
                            assert_ne!(
                                c.compare(a.as_bytes(), d.as_bytes()),
                                Ordering::Greater,
                                "transitivity {a:?} {b:?} {d:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// TABLE-28: root 照合の代表的な順序（ICU4X のデータに依存する基本的な性質だけを固定する）。
    #[test]
    fn und_icu_orders_basic_text() {
        let c = Collation::UndIcu;
        let mut v = vec!["B", "b", "a", "A", "ab", "\u{e9}", "e", "f"];
        v.sort_by(|x, y| c.compare(x.as_bytes(), y.as_bytes()));
        assert_eq!(v, ["a", "A", "ab", "b", "B", "e", "\u{e9}", "f"]);
        // バイト列順では大文字が小文字より前になる（ICU と異なる）。
        assert_eq!(Collation::C.compare(b"B", b"a"), Ordering::Less);
        assert_eq!(c.compare(b"B", b"a"), Ordering::Greater);
    }

    /// 長さ上限: 上限を超える入力は拒否される。
    #[test]
    fn sort_key_rejects_oversized_input() {
        let big = vec![b'a'; SORT_KEY_MAX_INPUT_BYTES + 1];
        let err = Collation::UndIcu.sort_key(&big).unwrap_err();
        assert!(matches!(err, SqlSurfaceError::PayloadTooLarge { .. }));
        // バイト列順は入力を借用するだけで上限を課さない。
        assert!(Collation::C.sort_key(&big).is_ok());
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
