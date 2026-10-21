//! GIN 相当索引（INDEX-8、Issue #2207）の索引エントリ抽出器。
//!
//! 親モジュール [`super`]（`gin_index.rs`）の書き込み経路（`constraint::enforce_row_constraints_in_txn`
//! 経由の DML 維持・`CREATE INDEX` の初回構築）が、jsonb 値 1 件から演算子クラスごとの索引エントリ
//! 集合を得るために呼ぶ純粋関数群。Issue #2208 の読み取り側（`sql::gin_scan`）も問い合わせ側の抽出に
//! 同じ規則（`query_*` 関数群。葉ハッシュ・経路符号化・値の正規化は格納側と共有）を使うため、
//! 抽出規則（`docs/design/gin-jsonb-index.md` D5・D6）を本ファイルに一本化する。
//!
//! # エントリ形式（永続形式の一部。変更時は [`super::FORMAT_VERSION`] を上げる）
//!
//! - `jsonb_ops`: キー `[0x01][UTF-8 キー]`／値 `[0x02][型タグ][payload]`。
//!   数値の payload は `json::scalar_equality_key` と同じ値としての正規化テキスト（`1`・`1.0`・
//!   `10e-1` が同一、`-0` は `0`）。最上位の文字列スカラーだけはキーと値の両方を出す。
//! - `jsonb_path_ops`: 葉スカラーごとに `[0x03][hash: u64 BE]`。ハッシュ入力はルートからのキー経路
//!   （各キーは長さ接頭辞付き）・値の型タグ・値の payload。配列は経路に段を足さない（透過）。
//!
//! # 上限（D7）
//!
//! 重複除去後のエントリ数が `GinLimits::max_keys_per_value`、`jsonb_ops` のエントリ 1 件のバイト長が
//! `GinLimits::max_key_bytes` を超えた時点で抽出を打ち切り [`GinExtractError::LimitExceeded`] を返す
//! （集合を無制限に伸ばさない）。呼び出し側は `54000` へ写像し、write txn を commit しない。

use crate::catalog::GinOpClass;
use crate::json::{scalar_equality_key, JsonValue};
use std::collections::BTreeSet;

/// 1 値あたりの抽出エントリ数（重複除去後）の既定上限。自動運転下の判断で、人間の確認が要る
/// （`docs/design/gin-jsonb-index.md` §8）。
pub(crate) const MAX_GIN_KEYS_PER_VALUE: usize = 65_535;

/// `jsonb_ops` のエントリ 1 件のバイト長の既定上限（`u16` の長さ接頭辞に収まる）。自動運転下の判断で、
/// 人間の確認が要る（`docs/design/gin-jsonb-index.md` §8）。
pub(crate) const MAX_GIN_KEY_BYTES: usize = 4_096;

/// 抽出上限。抽出器と `CREATE INDEX` の走査が受け取る。呼び出し元は [`GinLimits::default`] を渡す
/// （将来 CLI から下げる場合の差し込み口。フィールドは非公開で、既定値より大きくできない）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct GinLimits {
    max_keys_per_value: usize,
    max_key_bytes: usize,
}

impl Default for GinLimits {
    fn default() -> Self {
        Self {
            max_keys_per_value: MAX_GIN_KEYS_PER_VALUE,
            max_key_bytes: MAX_GIN_KEY_BYTES,
        }
    }
}

/// 抽出の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GinExtractError {
    /// エントリ数またはエントリのバイト長が上限を超えた（`54000`）。
    LimitExceeded,
    /// 格納値が jsonb として読めない（内部矛盾。fail-closed）。
    Invalid,
}

const KEY_TAG: u8 = 0x01;
const VALUE_TAG: u8 = 0x02;
const PATH_HASH_TAG: u8 = 0x03;

const TYPE_NULL: u8 = 0;
const TYPE_FALSE: u8 = 1;
const TYPE_TRUE: u8 = 2;
const TYPE_NUMBER: u8 = 3;
const TYPE_STRING: u8 = 4;

/// 格納済み jsonb テキストをパースして [`extract_entries`] へ渡す。パース失敗は
/// [`GinExtractError::Invalid`]。
pub(crate) fn extract_entries_from_text(
    opclass: GinOpClass,
    text: &str,
    limits: &GinLimits,
) -> Result<BTreeSet<Vec<u8>>, GinExtractError> {
    let value = crate::json::parse_json(text).map_err(|_| GinExtractError::Invalid)?;
    extract_entries(opclass, &value, limits)
}

/// jsonb 値 `value` から演算子クラス `opclass` の索引エントリ集合を抽出する（重複除去済み）。
pub(crate) fn extract_entries(
    opclass: GinOpClass,
    value: &JsonValue,
    limits: &GinLimits,
) -> Result<BTreeSet<Vec<u8>>, GinExtractError> {
    let mut out = Collector {
        set: BTreeSet::new(),
        limits: *limits,
    };
    match opclass {
        GinOpClass::JsonbOps => walk_ops(value, true, &mut out)?,
        GinOpClass::JsonbPathOps => {
            let mut path: Vec<u8> = Vec::new();
            walk_path(value, &mut path, &mut out)?;
        }
    }
    Ok(out.set)
}

struct Collector {
    set: BTreeSet<Vec<u8>>,
    limits: GinLimits,
}

impl Collector {
    fn push(&mut self, entry: Vec<u8>, check_bytes: bool) -> Result<(), GinExtractError> {
        if check_bytes && entry.len() > self.limits.max_key_bytes {
            return Err(GinExtractError::LimitExceeded);
        }
        self.set.insert(entry);
        if self.set.len() > self.limits.max_keys_per_value {
            return Err(GinExtractError::LimitExceeded);
        }
        Ok(())
    }
}

fn key_entry(key: &str, out: &mut Collector) -> Result<(), GinExtractError> {
    // 長いキーは確保する前に弾く（巨大な文字列キーでメモリを使わない）。
    if key.len().saturating_add(1) > out.limits.max_key_bytes {
        return Err(GinExtractError::LimitExceeded);
    }
    let mut e = Vec::with_capacity(1 + key.len());
    e.push(KEY_TAG);
    e.extend_from_slice(key.as_bytes());
    out.push(e, true)
}

/// スカラーの型タグと payload（文字列は生バイト、数値は正規化テキスト）。
fn scalar_parts(v: &JsonValue) -> Result<(u8, Vec<u8>), GinExtractError> {
    Ok(match v {
        JsonValue::Null => (TYPE_NULL, Vec::new()),
        JsonValue::Bool(false) => (TYPE_FALSE, Vec::new()),
        JsonValue::Bool(true) => (TYPE_TRUE, Vec::new()),
        JsonValue::Number(_) => (
            TYPE_NUMBER,
            scalar_equality_key(v)
                .map_err(|_| GinExtractError::Invalid)?
                .into_bytes(),
        ),
        JsonValue::String(s) => (TYPE_STRING, s.as_bytes().to_vec()),
        JsonValue::Array(_) | JsonValue::Object(_) => return Err(GinExtractError::Invalid),
    })
}

fn value_entry(v: &JsonValue, out: &mut Collector) -> Result<(), GinExtractError> {
    if let JsonValue::String(s) = v {
        if s.len().saturating_add(2) > out.limits.max_key_bytes {
            return Err(GinExtractError::LimitExceeded);
        }
    }
    let (tag, payload) = scalar_parts(v)?;
    let mut e = Vec::with_capacity(2 + payload.len());
    e.push(VALUE_TAG);
    e.push(tag);
    e.extend_from_slice(&payload);
    out.push(e, true)
}

/// `jsonb_ops` の再帰走査。`top` は jsonb 値そのものの階層（最上位の文字列スカラーだけキーも出す）。
fn walk_ops(v: &JsonValue, top: bool, out: &mut Collector) -> Result<(), GinExtractError> {
    match v {
        JsonValue::Object(map) => {
            for (k, val) in map {
                key_entry(k, out)?;
                match val {
                    JsonValue::Object(_) | JsonValue::Array(_) => walk_ops(val, false, out)?,
                    scalar => value_entry(scalar, out)?,
                }
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                match item {
                    JsonValue::String(s) => key_entry(s, out)?,
                    JsonValue::Object(_) | JsonValue::Array(_) => walk_ops(item, false, out)?,
                    scalar => value_entry(scalar, out)?,
                }
            }
        }
        JsonValue::String(s) => {
            value_entry(v, out)?;
            if top {
                key_entry(s, out)?;
            }
        }
        scalar => value_entry(scalar, out)?,
    }
    Ok(())
}

/// `jsonb_path_ops` の再帰走査。`path` はルートからのキー経路の長さ区切りエンコード。
fn walk_path(
    v: &JsonValue,
    path: &mut Vec<u8>,
    out: &mut Collector,
) -> Result<(), GinExtractError> {
    match v {
        JsonValue::Object(map) => {
            for (k, val) in map {
                let mark = path.len();
                push_path_segment(path, k);
                walk_path(val, path, out)?;
                path.truncate(mark);
            }
        }
        // 配列は経路に段を足さない（`jsonb_contains` の「配列 ⊇ スカラー」規則と整合させる）。
        JsonValue::Array(items) => {
            for item in items {
                walk_path(item, path, out)?;
            }
        }
        scalar => {
            let e = path_leaf_entry(path, scalar)?;
            // ハッシュ長は固定のため、バイト長上限は適用しない。
            out.push(e, false)?;
        }
    }
    Ok(())
}

/// `jsonb_path_ops` の経路キー 1 段（`[len: u64 BE][UTF-8 キー]`）を `path` へ足す。格納側
/// （[`walk_path`]）と問い合わせ側（[`query_path_leaf_entry`]）が同じ符号化を共有する
/// （第 2 の実装を作らない。偽陰性の根拠）。
fn push_path_segment(path: &mut Vec<u8>, key: &str) {
    path.extend_from_slice(&(key.len() as u64).to_be_bytes());
    path.extend_from_slice(key.as_bytes());
}

/// `jsonb_path_ops` の葉エントリ `[0x03][FNV-1a 64 BE]` を、経路 `path` とスカラー `scalar` から作る。
/// 格納側と問い合わせ側の共有関数（永続形式の一部。変更時は [`super::FORMAT_VERSION`] を上げる）。
fn path_leaf_entry(path: &[u8], scalar: &JsonValue) -> Result<Vec<u8>, GinExtractError> {
    let (tag, payload) = scalar_parts(scalar)?;
    let mut h = Fnv1a64::new();
    h.write(path);
    h.write(&[tag]);
    h.write(&(payload.len() as u64).to_be_bytes());
    h.write(&payload);
    let mut e = Vec::with_capacity(9);
    e.push(PATH_HASH_TAG);
    e.extend_from_slice(&h.finish().to_be_bytes());
    Ok(e)
}

/// 問い合わせ側が 1 回の探査で同時に開くポスティング範囲の上限。超える問い合わせは索引不可として
/// 全走査へ倒す（数万本の範囲反復器を同時に開く DoS を避ける。`sql::gin_scan` が呼ぶ）。
/// 自動運転下の判断で、人間の確認が要る（`docs/design/gin-jsonb-index.md` §9）。
pub(crate) const MAX_GIN_QUERY_KEYS: usize = 64;

/// 問い合わせキー集合を確定する共通の出口。空・上限超過は `None`（索引不可。全走査へ倒す）。
fn finish_query(set: BTreeSet<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    if set.is_empty() || set.len() > MAX_GIN_QUERY_KEYS {
        return None;
    }
    Some(set.into_iter().collect())
}

/// `col @> query` の問い合わせエントリ（積。すべてのエントリが必要）。`jsonb_contains(S, Q)` が真なら
/// 返り値は `extract_entries(S)` の部分集合になる（D5・D6）。`jsonb_ops` では、右辺が最上位の文字列
/// スカラーのときだけキー用タグのみを返す（`'["bar"]' @> '"bar"'` は格納側にキー用タグしか無い）。
/// 空の容器・上限超過・抽出不能は `None`（索引不可。「索引から全行」を返す経路を作らない）。
pub(crate) fn query_contains_entries(
    opclass: GinOpClass,
    query: &JsonValue,
    limits: &GinLimits,
) -> Option<Vec<Vec<u8>>> {
    let mut out = Collector {
        set: BTreeSet::new(),
        limits: *limits,
    };
    let result = match opclass {
        GinOpClass::JsonbOps => match query {
            JsonValue::String(s) => key_entry(s, &mut out),
            other => walk_ops(other, false, &mut out),
        },
        GinOpClass::JsonbPathOps => {
            let mut path: Vec<u8> = Vec::new();
            walk_path(query, &mut path, &mut out)
        }
    };
    result.ok()?;
    finish_query(out.set)
}

/// `col ? key`／`?|`／`?&` の問い合わせエントリ（キー用タグ `[0x01][key]`。`jsonb_ops` 専用）。
/// `keys` は NULL 要素を除いた後のキー列。空・上限超過・キー長超過は `None`（索引不可）。
pub(crate) fn query_key_entries<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    limits: &GinLimits,
) -> Option<Vec<Vec<u8>>> {
    let mut out = Collector {
        set: BTreeSet::new(),
        limits: *limits,
    };
    for key in keys {
        key_entry(key, &mut out).ok()?;
    }
    finish_query(out.set)
}

/// `jsonb_path_ops` の jsonpath 許可形（`@@`・`@?`）用の葉エントリ 1 件。`members` はルートからの
/// メンバー名の列（配列は透過なので添字・ワイルドカードは受け付けない）、`scalar` は比較リテラル
/// （容器は不可）。格納側と同じ [`push_path_segment`]・[`path_leaf_entry`] を通す。
pub(crate) fn query_path_leaf_entry<'a>(
    members: impl IntoIterator<Item = &'a str>,
    scalar: &JsonValue,
) -> Option<Vec<u8>> {
    let mut path: Vec<u8> = Vec::new();
    for m in members {
        push_path_segment(&mut path, m);
    }
    path_leaf_entry(&path, scalar).ok()
}

/// FNV-1a 64 ビット。永続形式の一部（`jsonb_path_ops` のエントリ）なので、アルゴリズムを変える場合は
/// [`super::FORMAT_VERSION`] を上げる。暗号強度は不要（衝突は読み取り側の再検査で除かれる）。
struct Fnv1a64(u64);

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(text: &str) -> BTreeSet<Vec<u8>> {
        extract_entries_from_text(GinOpClass::JsonbOps, text, &GinLimits::default()).unwrap()
    }

    fn path_ops(text: &str) -> BTreeSet<Vec<u8>> {
        extract_entries_from_text(GinOpClass::JsonbPathOps, text, &GinLimits::default()).unwrap()
    }

    fn k(s: &str) -> Vec<u8> {
        let mut e = vec![KEY_TAG];
        e.extend_from_slice(s.as_bytes());
        e
    }

    fn v(tag: u8, payload: &str) -> Vec<u8> {
        let mut e = vec![VALUE_TAG, tag];
        e.extend_from_slice(payload.as_bytes());
        e
    }

    #[test]
    fn fnv1a64_matches_published_vectors() {
        let mut h = Fnv1a64::new();
        assert_eq!(h.finish(), 0xcbf2_9ce4_8422_2325);
        h.write(b"a");
        assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c);
        let mut h = Fnv1a64::new();
        h.write(b"foobar");
        assert_eq!(h.finish(), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn jsonb_ops_numbers_share_one_entry_across_notations() {
        assert_eq!(ops(r#"{"a":1}"#), ops(r#"{"a":1.0}"#));
        assert_eq!(ops(r#"{"a":1}"#), ops(r#"{"a":10e-1}"#));
        assert_eq!(ops(r#"{"a":0}"#), ops(r#"{"a":-0}"#));
        assert_ne!(ops(r#"{"a":1}"#), ops(r#"{"a":2}"#));
    }

    #[test]
    fn jsonb_ops_top_level_string_emits_key_and_value() {
        let e = ops(r#""bar""#);
        assert!(e.contains(&k("bar")));
        assert!(e.contains(&v(TYPE_STRING, "bar")));
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn jsonb_ops_array_strings_are_keys_and_numbers_are_values() {
        let e = ops(r#"["x",1,null,true]"#);
        assert!(e.contains(&k("x")));
        assert!(!e.contains(&v(TYPE_STRING, "x")));
        assert!(e.contains(&v(TYPE_NUMBER, "1e0")));
        assert!(e.contains(&v(TYPE_NULL, "")));
        assert!(e.contains(&v(TYPE_TRUE, "")));
        assert!(!e.contains(&v(TYPE_FALSE, "")));
    }

    #[test]
    fn jsonb_ops_object_string_value_is_value_only_and_nested_keys_are_kept() {
        let e = ops(r#"{"a":{"b":"x"}}"#);
        assert!(e.contains(&k("a")));
        assert!(e.contains(&k("b")));
        assert!(e.contains(&v(TYPE_STRING, "x")));
        assert!(!e.contains(&k("x")));
    }

    #[test]
    fn jsonb_ops_deduplicates_within_a_value() {
        let e = ops(r#"{"a":{"a":1},"b":[1,1]}"#);
        assert_eq!(e.iter().filter(|x| **x == k("a")).count(), 1);
        assert_eq!(e.iter().filter(|x| **x == v(TYPE_NUMBER, "1e0")).count(), 1);
    }

    #[test]
    fn empty_containers_emit_no_entries() {
        assert!(ops("{}").is_empty());
        assert!(ops("[]").is_empty());
        assert!(path_ops("{}").is_empty());
        assert!(path_ops(r#"{"a":[]}"#).is_empty());
    }

    #[test]
    fn jsonb_path_ops_arrays_are_transparent() {
        assert_eq!(path_ops("[1]"), path_ops("1"));
        assert_eq!(path_ops(r#"{"a":[1]}"#), path_ops(r#"{"a":1}"#));
        assert_eq!(path_ops(r#"{"a":[[1]]}"#), path_ops(r#"{"a":1}"#));
    }

    #[test]
    fn jsonb_path_ops_distinguishes_paths_types_and_boundaries() {
        assert_ne!(path_ops(r#"{"a":1}"#), path_ops(r#"{"b":1}"#));
        assert_ne!(path_ops(r#"{"a":{"b":1}}"#), path_ops(r#"{"ab":1}"#));
        assert_ne!(path_ops(r#"{"a":"1"}"#), path_ops(r#"{"a":1}"#));
        assert_ne!(path_ops(r#"["a","bc"]"#), path_ops(r#"["ab","c"]"#));
        assert_eq!(path_ops(r#"{"a":1}"#), path_ops(r#"{"a":1.0}"#));
    }

    #[test]
    fn limits_accept_exact_and_reject_one_over() {
        let limits = GinLimits {
            max_keys_per_value: 3,
            max_key_bytes: 10,
        };
        // キー 2 + 値 1（null）= 3 件ちょうど。
        let three = r#"{"a":null,"b":null}"#;
        assert!(extract_entries_from_text(GinOpClass::JsonbOps, three, &limits).is_ok());
        // キー 2 + 値 2 = 4 件。
        let four = r#"{"a":1,"b":null}"#;
        assert_eq!(
            extract_entries_from_text(GinOpClass::JsonbOps, four, &limits),
            Err(GinExtractError::LimitExceeded)
        );
        // キー長: タグ 1 + 9 バイトちょうどは受理、10 バイトは拒否。
        let exact = format!(r#"{{"{}":null}}"#, "k".repeat(9));
        assert!(extract_entries_from_text(GinOpClass::JsonbOps, &exact, &limits).is_ok());
        let over = format!(r#"{{"{}":null}}"#, "k".repeat(10));
        assert_eq!(
            extract_entries_from_text(GinOpClass::JsonbOps, &over, &limits),
            Err(GinExtractError::LimitExceeded)
        );
    }

    #[test]
    fn path_ops_counts_entries_against_the_key_limit() {
        let limits = GinLimits {
            max_keys_per_value: 2,
            max_key_bytes: 10,
        };
        assert!(extract_entries_from_text(GinOpClass::JsonbPathOps, "[1,2]", &limits).is_ok());
        assert_eq!(
            extract_entries_from_text(GinOpClass::JsonbPathOps, "[1,2,3]", &limits),
            Err(GinExtractError::LimitExceeded)
        );
    }

    #[test]
    fn invalid_text_is_rejected_fail_closed() {
        assert_eq!(
            extract_entries_from_text(GinOpClass::JsonbOps, "{", &GinLimits::default()),
            Err(GinExtractError::Invalid)
        );
    }
    // ---- 問い合わせ側の抽出（Issue #2208）: 偽陰性なしの性質テスト ----

    const DOCS: &[&str] = &[
        r#"{}"#,
        r#"[]"#,
        r#"{"a":1}"#,
        r#"{"a":1.0}"#,
        r#"{"a":10e-1,"b":"x"}"#,
        r#"{"a":-0}"#,
        r#"{"a":0}"#,
        r#"{"a":"bar"}"#,
        r#"{"a":["bar","baz"]}"#,
        r#"{"a":{"b":{"c":true}}}"#,
        r#"{"a":[{"b":1},{"b":2,"c":null}]}"#,
        r#"{"a":[[1,2],[3]]}"#,
        r#"["foo","bar"]"#,
        r#"["foo",1,null,true,{"k":"v"}]"#,
        r#"[1,2,[1,3]]"#,
        r#"[[1,3]]"#,
        r#""bar""#,
        r#""foo""#,
        r#"1"#,
        r#"1.0"#,
        r#"true"#,
        r#"null"#,
        r#"{"tags":["a","b"],"meta":{"k":"v","n":[1,2,3]}}"#,
        r#"{"k":"v","k2":"v2","k3":{"x":[{"y":"z"}]}}"#,
        r#"[{"a":1,"b":2}]"#,
        r#"{"a":[]}"#,
        r#"{"a":{}}"#,
        r#"{"":""}"#,
        r#"{"a":"1"}"#,
        r#"{"ab":1}"#,
    ];

    const QUERIES: &[&str] = &[
        r#"{}"#,
        r#"[]"#,
        r#"{"a":1}"#,
        r#"{"a":1.0}"#,
        r#"{"a":10e-1}"#,
        r#"{"a":0}"#,
        r#"{"a":-0}"#,
        r#"{"a":"bar"}"#,
        r#"{"a":["bar"]}"#,
        r#"{"a":{"b":{}}}"#,
        r#"{"a":{"b":{"c":true}}}"#,
        r#"{"a":[{"b":1}]}"#,
        r#"{"a":[[1]]}"#,
        r#"{"a":[]}"#,
        r#"["bar"]"#,
        r#"["foo",1]"#,
        r#"[[1,3]]"#,
        r#"[1,3]"#,
        r#"[{"a":1}]"#,
        r#"[{"k":"v"}]"#,
        r#""bar""#,
        r#""foo""#,
        r#"1"#,
        r#"1.0"#,
        r#"true"#,
        r#"null"#,
        r#"{"tags":["a"]}"#,
        r#"{"meta":{"n":[2]}}"#,
        r#"{"k":"v"}"#,
        r#"{"":""}"#,
        r#"{"a":"1"}"#,
    ];

    fn parsed(text: &str) -> JsonValue {
        crate::json::parse_json(text).unwrap()
    }

    #[test]
    fn contains_query_entries_are_subset_of_stored_entries_whenever_contained() {
        let limits = GinLimits::default();
        for opclass in [GinOpClass::JsonbOps, GinOpClass::JsonbPathOps] {
            let mut matched = 0usize;
            for s in DOCS {
                let stored = extract_entries_from_text(opclass, s, &limits).unwrap();
                for q in QUERIES {
                    if !crate::json::jsonb_contains(s, q).unwrap() {
                        continue;
                    }
                    matched += 1;
                    let Some(keys) = query_contains_entries(opclass, &parsed(q), &limits) else {
                        continue;
                    };
                    for key in &keys {
                        assert!(
                            stored.contains(key),
                            "false negative: opclass={opclass:?} stored={s} query={q}"
                        );
                    }
                }
            }
            assert!(matched > 40, "corpus must exercise containment: {matched}");
        }
    }

    #[test]
    fn exists_query_entries_are_subset_of_jsonb_ops_entries_whenever_key_exists() {
        let limits = GinLimits::default();
        let keys = ["a", "b", "foo", "bar", "k", "tags", "x", "", "1", "v", "ab"];
        for s in DOCS {
            let stored = extract_entries_from_text(GinOpClass::JsonbOps, s, &limits).unwrap();
            for key in keys {
                if crate::json::jsonb_exists(s, key).unwrap() {
                    let q = query_key_entries([key], &limits).unwrap();
                    assert!(q.iter().all(|e| stored.contains(e)), "{s} ? {key}");
                }
            }
        }
    }

    #[test]
    fn top_level_string_query_uses_key_tag_only() {
        let limits = GinLimits::default();
        let q = query_contains_entries(GinOpClass::JsonbOps, &parsed(r#""bar""#), &limits).unwrap();
        assert_eq!(q, vec![k("bar")]);
        // 格納側の `["bar"]` はキー用タグしか持たないため、値用タグを要求してはならない。
        let stored = ops(r#"["bar"]"#);
        assert!(q.iter().all(|e| stored.contains(e)));
    }

    #[test]
    fn query_returns_none_for_empty_and_oversized_sets() {
        let limits = GinLimits::default();
        for opclass in [GinOpClass::JsonbOps, GinOpClass::JsonbPathOps] {
            assert!(query_contains_entries(opclass, &parsed("{}"), &limits).is_none());
            assert!(query_contains_entries(opclass, &parsed("[]"), &limits).is_none());
        }
        // 空の容器だけを持つ値は `jsonb_path_ops` では葉が無い（`jsonb_ops` はキー `a` が残る）。
        assert!(
            query_contains_entries(GinOpClass::JsonbPathOps, &parsed(r#"{"a":[]}"#), &limits)
                .is_none()
        );
        assert!(query_key_entries(std::iter::empty::<&str>(), &limits).is_none());
        let many: Vec<String> = (0..=MAX_GIN_QUERY_KEYS).map(|i| format!("k{i}")).collect();
        assert!(query_key_entries(many.iter().map(String::as_str), &limits).is_none());
        let ok: Vec<String> = (0..MAX_GIN_QUERY_KEYS).map(|i| format!("k{i}")).collect();
        assert!(query_key_entries(ok.iter().map(String::as_str), &limits).is_some());
        let big = format!(
            "[{}]",
            (0..=MAX_GIN_QUERY_KEYS)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(query_contains_entries(GinOpClass::JsonbPathOps, &parsed(&big), &limits).is_none());
        // 長すぎるキーは索引不可（fail-soft）。
        let long = "k".repeat(MAX_GIN_KEY_BYTES + 1);
        assert!(query_key_entries([long.as_str()], &limits).is_none());
    }

    #[test]
    fn path_leaf_query_matches_stored_leaf_entries() {
        let stored = path_ops(r#"{"a":{"b":"x"},"c":[1,2]}"#);
        let e = query_path_leaf_entry(["a", "b"], &JsonValue::String("x".to_string())).unwrap();
        assert!(stored.contains(&e));
        let e = query_path_leaf_entry(["c"], &parsed("2.0")).unwrap();
        assert!(stored.contains(&e));
        let e = query_path_leaf_entry(["a", "b"], &JsonValue::String("y".to_string())).unwrap();
        assert!(!stored.contains(&e));
        // 容器は葉になれない。
        assert!(query_path_leaf_entry(["a"], &parsed("[1]")).is_none());
    }

    #[test]
    fn number_equality_by_comparison_implies_same_equality_key() {
        use crate::json::cmp_json_numbers;
        let nums = [
            "0",
            "-0",
            "0.0",
            "1",
            "1.0",
            "10e-1",
            "1E0",
            "0.1e1",
            "100",
            "1e2",
            "1.00e2",
            "-1",
            "-1.0",
            "123456789012345678901234567890",
            "1.23456789012345678901234567890e29",
            "1e400",
            "1.0e400",
            "0.5",
            "5e-1",
            "-0.0e5",
        ];
        for a in nums {
            for b in nums {
                let (JsonValue::Number(x), JsonValue::Number(y)) = (parsed(a), parsed(b)) else {
                    panic!("not numbers");
                };
                if cmp_json_numbers(&x, &y) == Some(std::cmp::Ordering::Equal) {
                    assert_eq!(
                        scalar_equality_key(&JsonValue::Number(x)).unwrap(),
                        scalar_equality_key(&JsonValue::Number(y)).unwrap(),
                        "{a} vs {b}"
                    );
                }
            }
        }
    }
}
