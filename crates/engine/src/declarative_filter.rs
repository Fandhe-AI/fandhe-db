//! 宣言的メタデータフィルタ API（TASK-147・EXT-3。ポインタ:
//! `docs/spec/05-tasks.md` TASK-147・`docs/spec/04-behavior/extensions.md` EXT-3）。
//!
//! 責務境界: メタデータ列（`TEXT` 列）に対する**等価**・**前方一致**・
//! **`LIKE` 一般形**（中間一致・後方一致・`_` 1 文字ワイルドカード。SQL-24・
//! TASK-208、Issue #914）のフィルタを、任意の列名に対して宣言
//! （[`DeclarativeFilter`]）・スキーマへ束縛（[`bind`]/[`bind_all`]）・評価
//! （[`MetadataFilter::matches`]/[`matches_all`]）する。`LIKE` の一般形は
//! 二次索引が対応せず plain scan へ縮退する（`sql::scalar_plan`・
//! `sql::scalar_index` 参照）。
//!
//! 呼び出し文脈: `sql::allowlist::parse_where` が構文（`<col> = '<literal>'`・
//! `<col> LIKE '<prefix>%'`・`<col> (< | > | <= | >=) '<literal>'`）を
//! 許可リスト判定し、`sql::parser::bind_where_predicates` が本モジュールの
//! [`DeclarativeFilter`]・[`bind_all`] へ委譲してスキーマ照合済みの
//! [`MetadataFilter`] 列を得る。`sql::exec::execute_statement` の SCALAR 段
//! （RLS 事前フィルタを通過した可視行に対する事前適用・`HINT ORDER` で DISTANCE
//! 先行時の事後適用の両方）が [`matches_all`] を呼んで評価する（SQL-2 の等価条件の
//! 実装例を汎用化したもの）。
//!
//! `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較
//! （[`FilterOp::TypedCompare`]。TABLE-13・TASK-199、Issue #891）は、算術を
//! 持たない非数値型のみを対象にした宣言的経路（レーン B）として本モジュールへ
//! 追加した。INTEGER/BIGINT/REAL/DOUBLE 列を算術式・関数引数の中で使う経路
//! （レーン A）は式評価系（`sql::udf_call`・`sql::expr_program`）が担い、
//! 本モジュールの対象外のまま。
//!
//! `unwrap`/`expect`/添字アクセス `[]` を使わず `get()`・`strip_suffix`・`checked_*`
//! で untrusted なパターン文字列・列インデックスを扱う（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」）。

use crate::catalog::{ColumnType, TableSchema};
use crate::numeric::Decimal;
use crate::row_codec::{ScalarRef, MAX_TEXT_FIELD_LEN};
use crate::sql::allowlist::SqlSurfaceError;
use crate::uuid::Uuid;

/// 1 文（`SELECT`）が持てるメタデータフィルタ件数の上限。無制限 `Vec` 確保を避ける
/// （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」対応）。
/// `catalog::MAX_COLUMN_COUNT` と同値を採用する（1 列あたり複数フィルタを許すため
/// 列数と独立の定数だが、桁の妥当性は同じ方針に揃える）。
pub const MAX_METADATA_FILTERS: usize = 256;

/// `<col> [NOT] IN (...)` 1 個が持てる要素数の上限（SQL-24。TASK-208 ポインタ）。
/// `sql::allowlist::MAX_IN_LIST_ITEMS`（SQL テキスト経由の構文段）と同値を
/// 採用し、二重定義を避けるため同モジュールがこの定数を再エクスポートする形で
/// 参照する（NoSQL 表層 `wire-server::http::query::filter`（Issue #945・
/// NOSQL-14）が `in` 要素数上限の多層防御にも使う公開 API）。
pub const MAX_IN_LIST_ITEMS: usize = 256;

/// `LIKE` パターン（生パターン。エスケープ解除前）のバイト長上限
/// （SQL-24／TASK-208、Issue #914）。[`parse_like_pattern`] が確保・解析より
/// **前**に判定し、超過は `54000`。中間一致・後方一致を含む一般形は
/// [`LikePattern::matches`] の評価コストが O(n·m)（n = 値のバイト長、
/// m = パターン長）になるため、この上限で m を定数に抑え DoS を防ぐ
/// （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」）。
pub const MAX_LIKE_PATTERN_LEN: usize = 4096;

/// フィルタの意味論。等価はバイト列一致、前方一致は `str::starts_with` による
/// バイト前方一致（`prefix` 自体が構築時点で valid `str` のため UTF-8 境界は安全）。
/// いずれも大文字小文字を区別する（PG の `=`/`LIKE` の既定動作に倣う。曖昧な照合は
/// 持ち込まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterOp {
    Equals(String),
    StartsWith(String),
    /// BOOLEAN 列の等価条件（TABLE-13・TASK-196、Issue #883・D-c）。`Equals`/
    /// `StartsWith` は TEXT 列限定のまま据え置き、文字列比較（`flag = 'true'`）は
    /// 受理しない（fail-closed。`bind` が列型で振り分ける）。
    BoolEquals(bool),
    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較条件
    /// （TABLE-13・TASK-199、Issue #891・レーン B）。`value` は列型で解析済みの
    /// 型付きリテラルで、`bind` 時に一度だけ解析し評価時（[`MetadataFilter::matches`]）
    /// は解析し直さない。
    TypedCompare {
        op: CompareOp,
        value: TypedLiteral,
    },
    /// [`FilterOp::TypedCompare`] の未束縛（列型未確定）版。`sql::parser::
    /// bind_where_predicates` が `WherePredicate::Equality`（列型が
    /// Date/Timestamp/Numeric/Uuid/Bytea の場合。[`CompareOp::Eq`]）・
    /// `WherePredicate::Compare`（`< > <= >=`）の両方をここへ振り分ける。
    /// `bind_impl` が列型で [`CompareLiteral`] を解析し `TypedCompare` へ
    /// 確定させる。
    Compare {
        op: CompareOp,
        literal: CompareLiteral,
    },
    /// [`FilterOp::InList`]／[`FilterOp::InTyped`] の未束縛版（SQL-24。ポインタ:
    /// `docs/spec/05-tasks.md` TASK-208、`docs/spec/04-behavior/sql-surface.md`
    /// SQL-24）。要素は構文段が保持した生の文字列リテラル。`bind_impl` が列型で
    /// `InList`（TEXT／ENUM）・`InTyped`（DATE／TIMESTAMP／NUMERIC／UUID／BYTEA）
    /// のいずれかへ確定させる。
    InListLiteral {
        values: Vec<String>,
    },
    /// 束縛済み `IN` 条件（TEXT／ENUM 列）。値はソート・重複除去済み（二分探索で
    /// 評価する。[`MetadataFilter::eval`] 参照）。
    InText(Vec<String>),
    /// 束縛済み `IN` 条件（DATE／TIMESTAMP／NUMERIC／UUID／BYTEA 列）。
    InTyped(Vec<TypedLiteral>),
    /// [`FilterOp::Between`] の未束縛版。`low`／`high` は構文段が保持した生の
    /// 文字列リテラル。
    BetweenLiteral {
        low: String,
        high: String,
    },
    /// 束縛済み `BETWEEN` 条件（DATE／TIMESTAMP／NUMERIC／UUID／BYTEA 列）。
    /// `low > high` の場合は評価が常に偽になる（[`MetadataFilter::eval`] の
    /// `Ge`∧`Le` 判定が自然にそうなる。エラーにはしない。PG の `BETWEEN` と
    /// 同じ扱い）。
    Between {
        low: TypedLiteral,
        high: TypedLiteral,
    },
    /// 配列列の等価条件（TABLE-14・Issue #1193）。右辺リテラルは `bind` 時に列の
    /// [`crate::catalog::ArrayType`] で解析済み（NULL 要素どうしは等しい。
    /// PostgreSQL の `array_eq` と同じ）。二次索引は対応せず PlainScan へ縮退する。
    ArrayEquals(ArrayKey),
    /// 配列列の `IN`（Issue #1193）。要素数は [`MAX_IN_LIST_ITEMS`] で上限済み。
    InArray(Vec<ArrayKey>),
    /// `JSON`／`JSONB` 列の等価条件（TABLE-14・Issue #1193）。値は
    /// [`crate::json::canonical_equality_text`]（UNIQUE 制約と共通の値等価正規形）。
    /// 行側も評価時に同じ関数で正規化して比較する。
    JsonEquals(String),
    /// `JSON`／`JSONB` 列の `IN`（Issue #1193）。
    InJson(Vec<String>),
    /// `<col> IS NULL`。
    IsNull,
    /// `<col> IS NOT NULL`。
    IsNotNull,
    /// 前置・後置 `NOT` による否定。内側の評価結果を三値論理で反転する
    /// （UNKNOWN は UNKNOWN のまま。[`MetadataFilter::eval`] 参照）。
    Not(Box<FilterOp>),
    /// `TEXT` 列に対する `LIKE` の一般形（中間一致・後方一致・`_` 1 文字
    /// ワイルドカードを含む。SQL-24／TASK-208、Issue #914）。純粋な前方一致
    /// （末尾 `%` のみ）は [`FilterOp::StartsWith`] へ、ワイルドカードを
    /// 含まない完全一致は [`FilterOp::Equals`] へ束縛時に振り分けるため
    /// （[`parse_like_pattern`] 参照）、本 variant はそれ以外の一般形のみを
    /// 保持する。二次索引（`sql::scalar_index::ScalarIndex::candidates_for`）は
    /// 対応せず、`sql::scalar_plan::classify_scalar_plan` が常に
    /// `PlainScan` へ縮退させる（索引の有無で結果が変わらないための契約）。
    Like(LikePattern),
    /// [`FilterOp::Like`] の未束縛版（列名指定・生パターン未解析）。
    /// `sql::parser::bind_where_predicates` が `WherePredicate::Prefix`
    /// （名前は互換性のため据え置き。実体は LIKE の生パターン全般）から
    /// ここへ構築し、[`DeclarativeFilter::bind`] が列型検査後に
    /// [`parse_like_pattern`] を呼んで `Equals`／`StartsWith`／`Like` の
    /// いずれかへ確定させる。束縛済み [`MetadataFilter`] には現れない契約
    /// （`bind_all`/`bind_all_for_describe` は常にこの variant を解決済みに
    /// 変換する）。
    LikeUnbound(String),
}

/// 配列等価述語の束縛済み右辺（Issue #1193）。行バイトと同じ正準エンコード
/// （[`crate::row_codec::ArrayValue`] の `canonical_parts`）を保持し、行側の
/// [`crate::row_codec::ArrayRef`]（要素型・要素数・flags・ペイロード）とバイト
/// 単位で比較する。正準エンコードは値に対して単射（NULL ビットマップ・`-0.0`
/// 正規化を含む）のため、バイト一致が値の等価と一致する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayKey {
    elem: crate::catalog::ArrayElemType,
    count: u32,
    flags: u8,
    bytes: Vec<u8>,
}

impl ArrayKey {
    /// 束縛済み配列値から正準キーを作る。
    fn from_value(value: &crate::row_codec::ArrayValue) -> Result<Self, SqlSurfaceError> {
        let internal = |_| SqlSurfaceError::Internal {
            detail: "array literal could not be canonicalized".to_string(),
        };
        let (_, flags, bytes) = value.canonical_parts().map_err(internal)?;
        let count = u32::try_from(value.len()).map_err(|_| SqlSurfaceError::Internal {
            detail: "array literal element count overflow".to_string(),
        })?;
        Ok(Self {
            elem: value.elem(),
            count,
            flags,
            bytes,
        })
    }

    /// Prepared Describe（`$n` 由来のダミー値）用の空プレースホルダ。評価には
    /// 到達しない契約。
    fn placeholder(elem: crate::catalog::ArrayElemType) -> Self {
        Self {
            elem,
            count: 0,
            flags: 0,
            bytes: Vec::new(),
        }
    }

    /// 行側の配列値との等価判定。壊れた格納値（`JSON` 要素が JSON として読めない等）
    /// で正準化に失敗した場合は UNKNOWN（`None`。スカラー JSON 列の
    /// `json_canonical_of` と同じ fail-closed）。
    fn matches(&self, actual: &crate::row_codec::ArrayRef<'_>) -> Option<bool> {
        // `NUMERIC` 要素は精度・位取りが列宣言ごとに異なりうるため種別一致で比べ、
        // 値の等価は正準ペイロード（`JSON`／`NUMERIC` は値等価。Issue #1357 D4）で判定する。
        let payload = actual.equality_payload().ok()?;
        Some(
            crate::row_codec::array_elem_kind_eq(actual.elem(), self.elem)
                && actual.count() == self.count
                && actual.flags() == self.flags
                && payload.as_ref() == self.bytes.as_slice(),
        )
    }
}

/// `JSON`／`JSONB` 列の格納値を等価正規形へ変換して `expected` と比較する
/// （正規化に失敗する壊れた格納値は UNKNOWN。fail-closed）。
fn json_canonical_of(value: ScalarRef<'_>) -> Option<String> {
    match value {
        ScalarRef::Json(s) => crate::json::canonical_equality_text(s).ok(),
        _ => None,
    }
}

impl FilterOp {
    /// `value`（`None` は NULL）に対する三値論理での評価（SQL-24。TASK-208
    /// ポインタ）。`None`（UNKNOWN）は NULL 値・型不一致のいずれからも生じる
    /// （型不一致を `Some(false)` にすると `Not` で誤って真へ反転する
    /// ——fail-open——ため、UNKNOWN のまま維持する）。`IsNull`／`IsNotNull` のみ
    /// NULL そのものを判定対象とするため `value` が `None` でも `Some(bool)` を
    /// 返す。
    fn eval(&self, value: Option<ScalarRef<'_>>) -> Option<bool> {
        match self {
            // `as_dictionary_text` で TEXT／ENUM の両方を等価比較する
            // （Issue #890 D7。二次索引〔`sql::scalar_index`〕と同じ辞書表現）。
            FilterOp::Equals(expected) => {
                let v = value?;
                v.as_dictionary_text().map(|s| s == expected.as_str())
            }
            FilterOp::StartsWith(prefix) => {
                let v = value?;
                v.as_text().map(|s| s.starts_with(prefix.as_str()))
            }
            FilterOp::BoolEquals(expected) => {
                let v = value?;
                v.as_bool().map(|actual| actual == *expected)
            }
            FilterOp::TypedCompare {
                op,
                value: expected,
            } => {
                let v = value?;
                match expected {
                    TypedLiteral::Date(e) => v.as_date().map(|a| op.accepts(a.cmp(e))),
                    TypedLiteral::Timestamp(e) => v.as_timestamp().map(|a| op.accepts(a.cmp(e))),
                    TypedLiteral::Numeric(e) => v
                        .as_numeric()
                        .map(|a| op.accepts(crate::numeric::cmp_exact(&a, e))),
                    TypedLiteral::Uuid(e) => v.as_uuid().map(|a| op.accepts(a.cmp(e))),
                    TypedLiteral::Bytes(e) => v.as_bytes().map(|a| op.accepts(a.cmp(e.as_slice()))),
                }
            }
            FilterOp::InText(values) => {
                let v = value?;
                let text = v.as_dictionary_text()?;
                Some(
                    values
                        .binary_search_by(|probe| probe.as_str().cmp(text))
                        .is_ok(),
                )
            }
            FilterOp::InTyped(values) => {
                let v = value?;
                typed_in_eval(values, v)
            }
            FilterOp::Between { low, high } => {
                let v = value?;
                typed_between_eval(low, high, v)
            }
            // 配列・JSON 列の等価（Issue #1193）。型不一致・破損値は UNKNOWN。
            FilterOp::ArrayEquals(key) => match value? {
                ScalarRef::Array(a) => key.matches(&a),
                _ => None,
            },
            FilterOp::InArray(keys) => match value? {
                ScalarRef::Array(a) => {
                    // 破損した格納値は全キーに対して UNKNOWN（`NOT IN` 越しに誤って
                    // 真へ反転しない。fail-closed）。
                    let mut unknown = false;
                    for k in keys {
                        match k.matches(&a) {
                            Some(true) => return Some(true),
                            Some(false) => {}
                            None => unknown = true,
                        }
                    }
                    if unknown {
                        None
                    } else {
                        Some(false)
                    }
                }
                _ => None,
            },
            FilterOp::JsonEquals(expected) => {
                let canonical = json_canonical_of(value?)?;
                Some(canonical == *expected)
            }
            FilterOp::InJson(expected) => {
                let canonical = json_canonical_of(value?)?;
                Some(expected.contains(&canonical))
            }
            FilterOp::IsNull => Some(value.is_none()),
            FilterOp::IsNotNull => Some(value.is_some()),
            FilterOp::Not(inner) => inner.eval(value).map(|b| !b),
            // SQL-24／TASK-208、Issue #914: 中間一致・後方一致・`_` を含む
            // 一般形。`TEXT` 列限定（`bind_impl` が事前検査済み）で、
            // ENUM／VECTOR 等の型不一致値は `as_text()` が `None` を返すため
            // UNKNOWN として扱う（fail-closed。二値論理時代の `unwrap_or(false)`
            // と異なり、`NOT LIKE` 越しでも誤って真へ反転しない）。
            FilterOp::Like(pattern) => {
                let v = value?;
                v.as_text().map(|s| pattern.matches(s))
            }
            // 未束縛の値（`Compare`／`InListLiteral`／`BetweenLiteral`／
            // `LikeUnbound`）は `bind`/`bind_all` が常に束縛済み variant へ
            // 確定させるため評価に到達しない契約（fail-closed の保険腕）。
            FilterOp::Compare { .. }
            | FilterOp::InListLiteral { .. }
            | FilterOp::BetweenLiteral { .. }
            | FilterOp::LikeUnbound(_) => None,
        }
    }
}

/// [`FilterOp::InTyped`] の三値評価。要素はすべて同じ [`TypedLiteral`] variant
/// （`bind_filter_op` が列型ごとに統一して構築する契約）で、先頭要素の variant
/// から列の実測値 `v` を対応する型で読み出す。空リストは構文段
/// （`sql::allowlist::Parser::parse_in_list_body`）が拒否するため到達しない
/// （防御的に `Some(false)` とする）。
fn typed_in_eval(values: &[TypedLiteral], v: ScalarRef<'_>) -> Option<bool> {
    match values.first() {
        None => Some(false),
        Some(TypedLiteral::Date(_)) => {
            let actual = v.as_date()?;
            Some(
                values
                    .iter()
                    .any(|t| matches!(t, TypedLiteral::Date(e) if *e == actual)),
            )
        }
        Some(TypedLiteral::Timestamp(_)) => {
            let actual = v.as_timestamp()?;
            Some(
                values
                    .iter()
                    .any(|t| matches!(t, TypedLiteral::Timestamp(e) if *e == actual)),
            )
        }
        Some(TypedLiteral::Numeric(_)) => {
            let actual = v.as_numeric()?;
            Some(values.iter().any(|t| matches!(t, TypedLiteral::Numeric(e) if crate::numeric::cmp_exact(&actual, e) == std::cmp::Ordering::Equal)))
        }
        Some(TypedLiteral::Uuid(_)) => {
            let actual = v.as_uuid()?;
            Some(
                values
                    .iter()
                    .any(|t| matches!(t, TypedLiteral::Uuid(e) if *e == actual)),
            )
        }
        Some(TypedLiteral::Bytes(_)) => {
            let actual = v.as_bytes()?;
            Some(
                values
                    .iter()
                    .any(|t| matches!(t, TypedLiteral::Bytes(e) if e.as_slice() == actual)),
            )
        }
    }
}

/// [`FilterOp::Between`] の三値評価。`low`／`high` は同じ [`TypedLiteral`]
/// variant（`bind_filter_op` が同一列型から解析する契約）。`low > high` は
/// `Ge`∧`Le` の判定が自然に常時偽となる（エラーにはしない。PG の `BETWEEN` と
/// 同じ扱い）。
fn typed_between_eval(low: &TypedLiteral, high: &TypedLiteral, v: ScalarRef<'_>) -> Option<bool> {
    match (low, high) {
        (TypedLiteral::Date(lo), TypedLiteral::Date(hi)) => {
            let actual = v.as_date()?;
            Some(CompareOp::Ge.accepts(actual.cmp(lo)) && CompareOp::Le.accepts(actual.cmp(hi)))
        }
        (TypedLiteral::Timestamp(lo), TypedLiteral::Timestamp(hi)) => {
            let actual = v.as_timestamp()?;
            Some(CompareOp::Ge.accepts(actual.cmp(lo)) && CompareOp::Le.accepts(actual.cmp(hi)))
        }
        (TypedLiteral::Numeric(lo), TypedLiteral::Numeric(hi)) => {
            let actual = v.as_numeric()?;
            Some(
                CompareOp::Ge.accepts(crate::numeric::cmp_exact(&actual, lo))
                    && CompareOp::Le.accepts(crate::numeric::cmp_exact(&actual, hi)),
            )
        }
        (TypedLiteral::Uuid(lo), TypedLiteral::Uuid(hi)) => {
            let actual = v.as_uuid()?;
            Some(CompareOp::Ge.accepts(actual.cmp(lo)) && CompareOp::Le.accepts(actual.cmp(hi)))
        }
        (TypedLiteral::Bytes(lo), TypedLiteral::Bytes(hi)) => {
            let actual = v.as_bytes()?;
            Some(
                CompareOp::Ge.accepts(actual.cmp(lo.as_slice()))
                    && CompareOp::Le.accepts(actual.cmp(hi.as_slice())),
            )
        }
        // `bind_filter_op` は low/high を同じ列型で解析するため、異なる
        // variant の組み合わせは構築されない契約（fail-closed の保険腕）。
        _ => None,
    }
}

/// [`FilterOp::TypedCompare`]／[`FilterOp::Compare`] の比較演算子。
/// `sql::allowlist::CompareOp`（構文層。`< > <= >=` の字句表現のみ）とは別に
/// 本モジュール（意味層）で持つ。`Eq` は構文層に対応する variant を持たず、
/// `WherePredicate::Equality`（列型が非 TEXT/ENUM の場合）からのみ到達する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    /// `ordering`（値どうしの比較結果）がこの演算子を満たすかを判定する。
    /// `sql::scalar_index::ScalarIndex::candidates_for`（Issue #893。`NUMERIC`
    /// 以外の `TypedCompare` 列——`Date`/`Timestamp`/`Uuid`——の整数境界導出が
    /// 同じ演算子判定を共有するため `pub(crate)`）・`numeric::tests` の
    /// brute-force オラクルからも参照する。
    pub(crate) fn accepts(self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            CompareOp::Eq => ordering == Equal,
            CompareOp::Lt => ordering == Less,
            CompareOp::Le => ordering != Greater,
            CompareOp::Gt => ordering == Greater,
            CompareOp::Ge => ordering != Less,
        }
    }
}

/// [`FilterOp::Compare`] が保持する未解析リテラル。`Text` は文字列リテラル形
/// （`col > '...'`。SQL 表層の `sql::parser::bind_where_predicates` が
/// `WherePredicate::Compare`／`Equality` から構築する）。`Number` は
/// `NUMERIC` 列専用の裸数値リテラル形（`col > 1.5`）を表す
/// [`DeclarativeFilter::compare_numeric_literal`] 専用の variant で、
/// **Rust API 直接呼び出し限定**（TABLE-13・TASK-199、Issue #891・レーン B は
/// 文字列リテラル形のみを対象とするため、SQL 表層からは未結線。裸数値
/// リテラル形の SQL 構文追加はレーン A・別 Issue の対象）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareLiteral {
    Text(String),
    Number(String),
}

/// `sql::allowlist::CompareOp`（構文層）から本モジュールの意味表現へ写像する。
impl From<crate::sql::allowlist::CompareOp> for CompareOp {
    fn from(op: crate::sql::allowlist::CompareOp) -> Self {
        match op {
            crate::sql::allowlist::CompareOp::Lt => CompareOp::Lt,
            crate::sql::allowlist::CompareOp::Le => CompareOp::Le,
            crate::sql::allowlist::CompareOp::Gt => CompareOp::Gt,
            crate::sql::allowlist::CompareOp::Ge => CompareOp::Ge,
        }
    }
}

/// [`FilterOp::TypedCompare`] が保持する、列型で解析済みの範囲比較リテラル
/// （TABLE-13・TASK-199、Issue #891）。`bind` 時に一度だけ構築し、行ごとの
/// 評価（[`MetadataFilter::matches`]）では文字列解析をやり直さない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedLiteral {
    /// `DATE` 列。1970-01-01 起点の日数。
    Date(i32),
    /// `TIMESTAMP` 列。1970-01-01 00:00:00 起点のマイクロ秒。
    Timestamp(i64),
    /// `NUMERIC` 列。列の `scale` に丸めず、リテラル自身の小数桁数で解析した
    /// 正確な値（`numeric::cmp_exact` で比較する）。
    Numeric(Decimal),
    /// `UUID` 列。
    Uuid(Uuid),
    /// `BYTEA` 列。辞書順（バイト列の `Ord`）で比較する。
    Bytes(Vec<u8>),
}

/// 未束縛の宣言的フィルタ（列名指定）。SQL 経由（`sql::parser::bind_in_session`）・
/// Rust API 直接呼び出しの両方から構築できる（汎用 API としての利用形。
/// `DeclarativeFilter::starts_with("path", "src/").bind(&schema)` のように使う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativeFilter {
    column: String,
    op: FilterOp,
}

impl DeclarativeFilter {
    /// 等価フィルタを宣言する。
    pub fn equals(column: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Equals(value.into()),
        }
    }

    /// 前方一致フィルタを宣言する。`prefix` が空の場合は [`bind`](Self::bind) 時に
    /// `22000` で拒否する（無条件に真となる無意味なフィルタを黙って受理しない）。
    pub fn starts_with(column: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::StartsWith(prefix.into()),
        }
    }

    /// BOOLEAN 列の等価フィルタを宣言する（Issue #883・D-c）。
    pub fn bool_equals(column: impl Into<String>, value: bool) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::BoolEquals(value),
        }
    }

    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較フィルタを
    /// 文字列リテラル形（`col > '2024-01-01'` 等）で宣言する（TABLE-13・
    /// TASK-199、Issue #891）。列型に応じた解析は [`Self::bind`] 時に行う。
    pub fn compare(column: impl Into<String>, op: CompareOp, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Compare {
                op,
                literal: CompareLiteral::Text(value.into()),
            },
        }
    }

    /// `NUMERIC` 列専用: 裸の数値リテラル形（`col > 1.5`）の範囲比較フィルタを
    /// 宣言する（TABLE-13・TASK-199、Issue #891）。`raw` は引用符なしの数値
    /// テキストで、列の `scale` に丸めずリテラル自身の小数桁数で解析する
    /// （[`crate::numeric::parse_literal_exact`]）。
    pub fn compare_numeric_literal(
        column: impl Into<String>,
        op: CompareOp,
        raw: impl Into<String>,
    ) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::Compare {
                op,
                literal: CompareLiteral::Number(raw.into()),
            },
        }
    }

    /// `<col> [NOT] IN (...)` フィルタを文字列リテラル形で宣言する（SQL-24。
    /// TASK-208 ポインタ）。列型に応じた解析（TEXT／ENUM は辞書等価、
    /// DATE／TIMESTAMP／NUMERIC／UUID／BYTEA は型付き等価）は [`Self::bind`] 時に
    /// 行う。
    pub fn in_list(column: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::InListLiteral { values },
        }
    }

    /// `<col> [NOT] BETWEEN '<low>' AND '<high>'` フィルタを文字列リテラル形で
    /// 宣言する（SQL-24。TASK-208 ポインタ）。`DATE`／`TIMESTAMP`／`NUMERIC`／
    /// `UUID`／`BYTEA` 列のみ受理し、他の列型は [`Self::bind`] 時に `22000`。
    pub fn between(
        column: impl Into<String>,
        low: impl Into<String>,
        high: impl Into<String>,
    ) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::BetweenLiteral {
                low: low.into(),
                high: high.into(),
            },
        }
    }

    /// `<col> IS NULL` フィルタを宣言する（SQL-24。TASK-208 ポインタ）。
    /// `VECTOR` 列は [`Self::bind`] 時に `22000`。
    pub fn is_null(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::IsNull,
        }
    }

    /// `<col> IS NOT NULL` フィルタを宣言する（SQL-24。TASK-208 ポインタ）。
    pub fn is_not_null(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::IsNotNull,
        }
    }

    /// 自身を否定した新しいフィルタを返す（SQL-24。TASK-208 ポインタ）。
    /// [`sql::parser::bind_where_predicates`] が `WherePredicate::Not` を
    /// 束縛する際、内側を先に構築してから本メソッドで包む。
    pub fn negate(self) -> Self {
        Self {
            column: self.column,
            op: FilterOp::Not(Box::new(self.op)),
        }
    }

    /// 否定を畳み込んだ形で返す（Issue #1197。`sql::declarative_predicate::
    /// negate_conjunction` が葉の否定に使う）。`Not(x)` は `x` へ戻し（二重否定の
    /// 除去。[`FilterOp::Not`] の入れ子を作らない）、`IS NULL`／`IS NOT NULL` は
    /// 互いへ反転する（この 2 つは UNKNOWN にならず厳密に同値。
    /// `sql::where_negation` と同じ判断）。それ以外は [`Self::negate`] で包む。
    pub(crate) fn negate_folded(self) -> Self {
        let column = self.column;
        let op = match self.op {
            FilterOp::Not(inner) => *inner,
            FilterOp::IsNull => FilterOp::IsNotNull,
            FilterOp::IsNotNull => FilterOp::IsNull,
            other => FilterOp::Not(Box::new(other)),
        };
        Self { column, op }
    }

    /// `TEXT` 列に対する `LIKE` フィルタを宣言する（SQL-24／TASK-208、
    /// Issue #914）。`pattern` は生パターン（エスケープ解除前）で、
    /// [`Self::bind`] 時に [`parse_like_pattern`] で解析し、
    /// `Equals`／`StartsWith`／`Like` のいずれかへ確定させる（振り分けの
    /// 詳細は同関数のドキュメント参照）。
    pub fn like(column: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op: FilterOp::LikeUnbound(pattern.into()),
        }
    }

    /// `schema` と照合して [`MetadataFilter`] へ束縛する。列名解決・列型検査
    /// （`Equals`/`StartsWith` は `TEXT` 列限定・`BoolEquals` は `BOOLEAN` 列限定。
    /// いずれも不一致は `22000`）・リテラル長上限（[`MAX_TEXT_FIELD_LEN`] 超は
    /// `54000`）・空 prefix 拒否（`22000`）を検証する。
    pub fn bind(&self, schema: &TableSchema) -> Result<MetadataFilter, SqlSurfaceError> {
        self.bind_impl(schema, false)
    }

    /// [`Self::bind`] の内部実装。`skip_enum_label_validation` が `true` の
    /// ときに限り、ENUM 列の等価フィルタで語彙照合（`EnumTypeDef::
    /// validate_label`）を省略する。Prepared Describe（`$n` 由来のダミー値。
    /// PR #1012・Cursor Bugbot 指摘対応）専用の縮退経路であり、公開 API
    /// [`Self::bind`]／[`bind_all`] は常に `false`（従来どおり全値検証）を渡す
    /// （`sql::parser::bind_where_predicates` の `dummy_equality_flags` 経由の
    /// み `true` になりうる。詳細は同関数のドキュメント参照）。列型検査・
    /// リテラル長上限・空 prefix 拒否など値に依存しない構造検証は
    /// `skip_enum_label_validation` の値に関係なく常に行う。
    fn bind_impl(
        &self,
        schema: &TableSchema,
        skip_enum_label_validation: bool,
    ) -> Result<MetadataFilter, SqlSurfaceError> {
        let column_index = schema
            .columns
            .iter()
            .position(|c| c.name == self.column)
            .ok_or_else(|| {
                SqlSurfaceError::invalid_input(format!("unknown column: {}", self.column))
            })?;
        let column = schema.columns.get(column_index).ok_or_else(|| {
            SqlSurfaceError::invalid_input(format!("unknown column: {}", self.column))
        })?;
        let op = bind_filter_op(
            &self.op,
            &self.column,
            &column.ty,
            skip_enum_label_validation,
        )?;
        Ok(MetadataFilter { column_index, op })
    }
}

/// [`DeclarativeFilter::bind_impl`] の本体（列型検査・リテラル解析）。
/// [`FilterOp::Not`] は内側を再帰でそのまま束縛する（構文段の不変条件により
/// 深さは常に 1 のため無限再帰は起きない。`sql::allowlist::WherePredicate::Not`
/// のドキュメント参照）。
fn bind_filter_op(
    op: &FilterOp,
    column_name: &str,
    ty: &ColumnType,
    skip_enum_label_validation: bool,
) -> Result<FilterOp, SqlSurfaceError> {
    Ok(match op {
        FilterOp::Equals(value) => {
            // 配列・JSON／JSONB 列の等価（TABLE-14・Issue #1193）。リテラルは
            // 列型の入力文法で解析し、Describe 縮退時（`$n` 由来のダミー）は
            // 解析せずプレースホルダを返す。
            match ty {
                ColumnType::Array(array_ty) => {
                    return Ok(if skip_enum_label_validation {
                        FilterOp::ArrayEquals(ArrayKey::placeholder(array_ty.elem()))
                    } else {
                        FilterOp::ArrayEquals(bind_array_key(value, array_ty)?)
                    });
                }
                ColumnType::Json | ColumnType::Jsonb => {
                    return Ok(if skip_enum_label_validation {
                        FilterOp::JsonEquals(String::new())
                    } else {
                        FilterOp::JsonEquals(bind_json_key(value)?)
                    });
                }
                _ => {}
            }
            // ENUM 列は TEXT と同じ等価述語を受理する（Issue #890 D7。
            // PostgreSQL の enum 入力と同様、語彙外のラベルは書き込み時と
            // 同じ `22P02` で拒否する。二次索引〔`sql::scalar_index`〕は
            // TEXT と同じ辞書を共有するため、この等価意味論のまま
            // 索引経由の候補削減を信頼できる）。
            match ty {
                ColumnType::Text => {}
                ColumnType::Enum(def) => {
                    // `skip_enum_label_validation` が `true` の場合、この値は
                    // `sql::params::substitute_dummy` が生成した固定ダミー
                    // 文字列であり、実際にどのラベルが束縛されるかは Bind
                    // まで未確定（PR #1012 Cursor Bugbot 指摘: ここで通常どおり
                    // 語彙照合すると、`WHERE enum_col = $n` を含む文の Describe
                    // が実リテラルの有無に関わらず常に `22P02` になってしまう）。
                    if !skip_enum_label_validation && def.validate_label(value).is_err() {
                        return Err(SqlSurfaceError::invalid_text_representation(format!(
                            "column {column_name:?} (enum {:?}) does not accept label {value:?}",
                            def.name()
                        )));
                    }
                }
                // F10（Issue #882 計画）: REAL/DOUBLE 列は VECTOR 列と同じ
                // 「TEXT 列でない」拒否腕へ合流させる（対応は #891 へ申し送り）。
                ColumnType::Vector(_)
                | ColumnType::Integer
                | ColumnType::BigInt
                | ColumnType::Real
                | ColumnType::Double
                | ColumnType::Boolean
                | ColumnType::Date
                | ColumnType::Timestamp
                | ColumnType::Array(_)
                | ColumnType::Bytea
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Numeric { .. }
                | ColumnType::Uuid => {
                    return Err(SqlSurfaceError::invalid_input(format!(
                        "column {column_name:?} is not a TEXT column"
                    )));
                }
            }
            check_literal_len(value)?;
            FilterOp::Equals(value.clone())
        }
        FilterOp::StartsWith(prefix) => {
            if !matches!(ty, ColumnType::Text) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column_name:?} is not a TEXT column"
                )));
            }
            if prefix.is_empty() {
                return Err(SqlSurfaceError::invalid_input(
                    "LIKE prefix must not be empty",
                ));
            }
            check_literal_len(prefix)?;
            FilterOp::StartsWith(prefix.clone())
        }
        FilterOp::LikeUnbound(pattern) => {
            // SQL-24／TASK-208、Issue #914: `TEXT` 列限定（`Equals`／
            // `StartsWith` と同じ制約）。ENUM／VECTOR／BOOLEAN 等は
            // 従来どおり `22000`。
            if !matches!(ty, ColumnType::Text) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column_name:?} is not a TEXT column"
                )));
            }
            match parse_like_pattern(pattern)? {
                // 索引を最大限利用するため、ワイルドカードを含まない
                // 完全一致・純粋な前方一致（末尾 `%` のみ）は既存の
                // `Equals`／`StartsWith` へ振り分ける（`sql::scalar_index`
                // が引き続き `index_equality`／`index_prefix` を提供する）。
                CompiledLike::Exact(literal) => {
                    check_literal_len(&literal)?;
                    FilterOp::Equals(literal)
                }
                CompiledLike::Prefix(prefix) => {
                    check_literal_len(&prefix)?;
                    FilterOp::StartsWith(prefix)
                }
                CompiledLike::General(pattern) => FilterOp::Like(pattern),
            }
        }
        FilterOp::BoolEquals(value) => {
            if !matches!(ty, ColumnType::Boolean) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column_name:?} is not a BOOLEAN column"
                )));
            }
            FilterOp::BoolEquals(*value)
        }
        FilterOp::Compare {
            op: cmp_op,
            literal,
        } => {
            // `skip_enum_label_validation` を「型付きリテラル解析の
            // スキップ」へ一般化する（Issue #891。ENUM の語彙照合スキップ
            // と同じ理由: `sql::params::substitute_dummy` が生成する固定
            // ダミー文字列 `"0"` は DATE／TIMESTAMP／UUID／BYTEA の文法として
            // 不正なため、Describe 時点でこれを実際に解析すると
            // `WHERE date_col = $1` 等の Describe が常に失敗してしまう。
            // 実際の値検証は Bind／Execute で行われる（他の列型と同じ
            // 縮退方針）。プレースホルダ値は評価（`matches`）に到達しない
            // 契約（Describe は検索本体を実行しない）。
            let value = if skip_enum_label_validation {
                match ty {
                    ColumnType::Date => TypedLiteral::Date(0),
                    ColumnType::Timestamp => TypedLiteral::Timestamp(0),
                    ColumnType::Numeric { .. } => TypedLiteral::Numeric(
                        crate::numeric::Decimal::from_parts(0, 0).map_err(|_| {
                            // `scale=0` は `MAX_PRECISION` 以下のため理論上
                            // 到達しないが、engine ライブラリコードは panic
                            // させない契約（`.claude/rules/coding-rust.md`）
                            // のため fail-closed に `Result` で伝播する。
                            SqlSurfaceError::Internal {
                                detail: "Decimal::from_parts(0, 0) must always succeed".to_string(),
                            }
                        })?,
                    ),
                    ColumnType::Uuid => {
                        TypedLiteral::Uuid(crate::uuid::Uuid::from_bytes([0u8; 16]))
                    }
                    ColumnType::Bytea => TypedLiteral::Bytes(Vec::new()),
                    _ => return Err(unsupported_compare_column(column_name)),
                }
            } else {
                bind_typed_compare_literal(column_name, ty, literal)?
            };
            FilterOp::TypedCompare { op: *cmp_op, value }
        }
        FilterOp::TypedCompare { .. }
        | FilterOp::InText(_)
        | FilterOp::InTyped(_)
        | FilterOp::ArrayEquals(_)
        | FilterOp::InArray(_)
        | FilterOp::JsonEquals(_)
        | FilterOp::InJson(_)
        | FilterOp::Between { .. }
        | FilterOp::Like(_) => {
            // `DeclarativeFilter` の公開コンストラクタはいずれも未束縛の
            // 値（`Compare`／`InListLiteral`／`BetweenLiteral`／`LikeUnbound`）を
            // 生成し、束縛済み variant を直接構築する経路は無い（fail-closed の
            // 保険腕。`bind`/`bind_all` は常に未束縛の値を受け取る契約）。
            return Err(SqlSurfaceError::Internal {
                detail:
                    "DeclarativeFilter must not be constructed with an already-bound filter value"
                        .to_string(),
            });
        }
        // `<col> [NOT] IN ('<lit>'[, ...])`（SQL-24。TASK-208 ポインタ）。
        // TEXT／ENUM は辞書等価（`InText`）、DATE／TIMESTAMP／NUMERIC／UUID／
        // BYTEA は型付き等価（`InTyped`）へ確定させる。ソート・重複除去は
        // `InText` のみ行う（索引側の候補集合〔`sql::scalar_index`〕が辞書
        // スロットの昇順連結を前提とするため。`InTyped` は索引未対応
        // 〔`sql::scalar_plan::classify_scalar_plan` が常に `PlainScan` へ
        // 倒す〕のためソート不要）。
        FilterOp::InListLiteral { values } => {
            // `sql::allowlist::parse_in_list_body`（SQL テキスト経由）は構文段で
            // 既に `MAX_IN_LIST_ITEMS` を検査済みだが、`DeclarativeFilter::
            // in_list` は engine の公開 API であり、SQL テキストを経由しない
            // 呼び出し元（`sql::declarative_predicate::bind_declarative_predicates`
            // の直接呼び出し等）は構文段の検査を経ない。ここが `Vec::with_capacity`
            // より前の唯一の共通防御点になるため、束縛の最初に必ず検査する
            // （`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
            // （DoS）」対応）。
            if values.len() > MAX_IN_LIST_ITEMS {
                return Err(SqlSurfaceError::payload_too_large(format!(
                    "IN list item count exceeds limit {MAX_IN_LIST_ITEMS}"
                )));
            }
            match ty {
                ColumnType::Text => {
                    let mut bound = Vec::with_capacity(values.len());
                    for v in values {
                        check_literal_len(v)?;
                        bound.push(v.clone());
                    }
                    bound.sort();
                    bound.dedup();
                    FilterOp::InText(bound)
                }
                ColumnType::Enum(def) => {
                    let mut bound = Vec::with_capacity(values.len());
                    for v in values {
                        check_literal_len(v)?;
                        if !skip_enum_label_validation && def.validate_label(v).is_err() {
                            return Err(SqlSurfaceError::invalid_text_representation(format!(
                                "column {column_name:?} (enum {:?}) does not accept label {v:?}",
                                def.name()
                            )));
                        }
                        bound.push(v.clone());
                    }
                    bound.sort();
                    bound.dedup();
                    FilterOp::InText(bound)
                }
                ColumnType::Date
                | ColumnType::Timestamp
                | ColumnType::Numeric { .. }
                | ColumnType::Uuid
                | ColumnType::Bytea => {
                    let mut bound = Vec::with_capacity(values.len());
                    for v in values {
                        let literal = CompareLiteral::Text(v.clone());
                        bound.push(if skip_enum_label_validation {
                            match ty {
                                ColumnType::Date => TypedLiteral::Date(0),
                                ColumnType::Timestamp => TypedLiteral::Timestamp(0),
                                ColumnType::Numeric { .. } => TypedLiteral::Numeric(
                                    Decimal::from_parts(0, 0).map_err(|_| {
                                        SqlSurfaceError::Internal {
                                            detail: "Decimal::from_parts(0, 0) must always succeed"
                                                .to_string(),
                                        }
                                    })?,
                                ),
                                ColumnType::Uuid => {
                                    TypedLiteral::Uuid(crate::uuid::Uuid::from_bytes([0u8; 16]))
                                }
                                ColumnType::Bytea => TypedLiteral::Bytes(Vec::new()),
                                _ => return Err(unsupported_compare_column(column_name)),
                            }
                        } else {
                            bind_typed_compare_literal(column_name, ty, &literal)?
                        });
                    }
                    FilterOp::InTyped(bound)
                }
                // 配列・JSON／JSONB 列の `IN`（Issue #1193）。要素数は上で検査済み。
                ColumnType::Array(array_ty) => {
                    let mut bound = Vec::with_capacity(values.len());
                    for v in values {
                        bound.push(if skip_enum_label_validation {
                            ArrayKey::placeholder(array_ty.elem())
                        } else {
                            bind_array_key(v, array_ty)?
                        });
                    }
                    FilterOp::InArray(bound)
                }
                ColumnType::Json | ColumnType::Jsonb => {
                    let mut bound = Vec::with_capacity(values.len());
                    for v in values {
                        bound.push(if skip_enum_label_validation {
                            String::new()
                        } else {
                            bind_json_key(v)?
                        });
                    }
                    FilterOp::InJson(bound)
                }
                _ => return Err(unsupported_compare_column(column_name)),
            }
        }
        // `<col> [NOT] BETWEEN '<low>' AND '<high>'`（SQL-24。TASK-208
        // ポインタ）。`low > high` は解析時にエラーにしない（評価時に
        // `Ge`∧`Le` が自然に常時偽となる。PG の `BETWEEN` と同じ扱い）。
        FilterOp::BetweenLiteral { low, high } => match ty {
            ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid
            | ColumnType::Bytea => {
                if skip_enum_label_validation {
                    let dummy = match ty {
                        ColumnType::Date => TypedLiteral::Date(0),
                        ColumnType::Timestamp => TypedLiteral::Timestamp(0),
                        ColumnType::Numeric { .. } => {
                            TypedLiteral::Numeric(Decimal::from_parts(0, 0).map_err(|_| {
                                SqlSurfaceError::Internal {
                                    detail: "Decimal::from_parts(0, 0) must always succeed"
                                        .to_string(),
                                }
                            })?)
                        }
                        ColumnType::Uuid => {
                            TypedLiteral::Uuid(crate::uuid::Uuid::from_bytes([0u8; 16]))
                        }
                        ColumnType::Bytea => TypedLiteral::Bytes(Vec::new()),
                        _ => return Err(unsupported_compare_column(column_name)),
                    };
                    FilterOp::Between {
                        low: dummy.clone(),
                        high: dummy,
                    }
                } else {
                    let low = bind_typed_compare_literal(
                        column_name,
                        ty,
                        &CompareLiteral::Text(low.clone()),
                    )?;
                    let high = bind_typed_compare_literal(
                        column_name,
                        ty,
                        &CompareLiteral::Text(high.clone()),
                    )?;
                    FilterOp::Between { low, high }
                }
            }
            _ => return Err(unsupported_compare_column(column_name)),
        },
        // `<col> IS [NOT] NULL`（SQL-24。TASK-208 ポインタ）。`VECTOR` 列は
        // 拒否する（`row_codec::scan_scalar_columns_masked` がマスク外・
        // `VECTOR` 列に常に `None` を積むため、評価させると fail-open
        // 〔存在しない NULL 行が誤って一致〕になり得る。TASK-208 実装計画
        // §4.3 のマスク網羅確認と対をなす防御）。
        FilterOp::IsNull => {
            if matches!(ty, ColumnType::Vector(_)) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column_name:?} does not support IS NULL (VECTOR column)"
                )));
            }
            FilterOp::IsNull
        }
        FilterOp::IsNotNull => {
            if matches!(ty, ColumnType::Vector(_)) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {column_name:?} does not support IS NOT NULL (VECTOR column)"
                )));
            }
            FilterOp::IsNotNull
        }
        // 前置・後置 `NOT`（SQL-24。TASK-208 ポインタ）。内側を再帰で
        // 束縛してから包む。
        FilterOp::Not(inner) => FilterOp::Not(Box::new(bind_filter_op(
            inner,
            column_name,
            ty,
            skip_enum_label_validation,
        )?)),
    })
}

/// 配列等価・`IN` の右辺リテラルを列の配列型で解析して正準キーへ変換する
/// （Issue #1193）。解析は INSERT と同じ [`crate::sql::parser::parse_array_literal`]
/// を共有し、長さ上限・形式違反・要素型ごとのエラー分類（`54000`／`22P02`／
/// `22003`／`22007`／`22008`）を書き込み経路と揃える。
fn bind_array_key(
    literal: &str,
    array_ty: &crate::catalog::ArrayType,
) -> Result<ArrayKey, SqlSurfaceError> {
    let value = crate::sql::parser::parse_array_literal(literal, array_ty)?;
    ArrayKey::from_value(&value)
}

/// JSON／JSONB 等価・`IN` の右辺リテラルを値等価の正規形へ変換する
/// （Issue #1193）。長さ上限・構文・深さ・要素数は
/// [`crate::json::canonical_equality_text`]（UNIQUE 制約と共通）が検証する。
fn bind_json_key(literal: &str) -> Result<String, SqlSurfaceError> {
    crate::json::canonical_equality_text(literal)
        .map_err(|e| crate::sql::parser::json_column_error(e, literal))
}

/// 範囲比較（[`FilterOp::Compare`]）を受理しない列型へ束縛しようとした場合の
/// エラー（`22000`）。
fn unsupported_compare_column(column: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_input(format!(
        "column {column:?} does not support range comparison (expected DATE/TIMESTAMP/NUMERIC/UUID/BYTEA)"
    ))
}

/// [`FilterOp::Compare`] の未解析リテラルを列型 `ty` へ束縛し、
/// [`TypedLiteral`] を構築する（TABLE-13・TASK-199、Issue #891）。INSERT／
/// UPDATE／UPSERT と同じ `sql::parser::bind_*_literal` 群を共有し、第 2の
/// パーサーを作らない。
fn bind_typed_compare_literal(
    column: &str,
    ty: &ColumnType,
    literal: &CompareLiteral,
) -> Result<TypedLiteral, SqlSurfaceError> {
    match (ty, literal) {
        (ColumnType::Date, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_datetime_literal(column, ColumnType::Date, s)? {
                crate::row_codec::Value::Date(d) => Ok(TypedLiteral::Date(d)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_datetime_literal returned a non-Date value for a DATE column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Timestamp, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_datetime_literal(column, ColumnType::Timestamp, s)? {
                crate::row_codec::Value::Timestamp(t) => Ok(TypedLiteral::Timestamp(t)),
                _ => Err(SqlSurfaceError::Internal {
                    detail:
                        "bind_datetime_literal returned a non-Timestamp value for a TIMESTAMP column"
                            .to_string(),
                }),
            }
        }
        (ColumnType::Uuid, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            match crate::sql::parser::bind_uuid_literal(s, column)? {
                crate::row_codec::Value::Uuid(u) => Ok(TypedLiteral::Uuid(u)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_uuid_literal returned a non-Uuid value for a UUID column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Bytea, CompareLiteral::Text(s)) => {
            // `check_literal_len` は使わない: `BYTEA` の `s` は復号後バイト列を
            // hex テキスト化した表現（`typed_json::bytea_literal_text`）で
            // 長さが約 2 倍に膨らむため、`MAX_TEXT_FIELD_LEN`（復号後基準の
            // `MAX_BYTEA_FIELD_LEN` と同値）をテキスト長へそのまま適用すると
            // insert/update で受理できる復号後約 2〜4 MiB の値が eq/範囲比較
            // フィルタでは `54000` になり書き込みと検索の許容範囲が食い違う
            // （Issue #896・PR #1038 レビュー指摘）。`bind_bytea_literal`
            // （`bytea::parse_hex_text`）自身が確保前に復号後長で
            // `MAX_BYTEA_FIELD_LEN` 超過を判定し `TooLong` を返すため、ここでの
            // 事前検査は不要かつ有害。
            match crate::sql::parser::bind_bytea_literal(s, column)? {
                crate::row_codec::Value::Bytes(b) => Ok(TypedLiteral::Bytes(b)),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "bind_bytea_literal returned a non-Bytes value for a BYTEA column"
                        .to_string(),
                }),
            }
        }
        (ColumnType::Numeric { .. }, CompareLiteral::Text(s)) => {
            check_literal_len(s)?;
            crate::numeric::parse_literal_exact(s)
                .map(TypedLiteral::Numeric)
                .map_err(|e| numeric_literal_error(column, e))
        }
        (ColumnType::Numeric { .. }, CompareLiteral::Number(raw)) => {
            check_literal_len(raw)?;
            crate::numeric::parse_literal_exact(raw)
                .map(TypedLiteral::Numeric)
                .map_err(|e| numeric_literal_error(column, e))
        }
        // `Number`（裸の数値リテラル）形は NUMERIC 列専用（`sql::parser::
        // bind_where_predicates` が振り分ける）。Date/Timestamp/Uuid/Bytea へ
        // 数値リテラルで比較しようとした場合、および Text/Vector/Integer/
        // BigInt/Real/Double/Boolean/Array/Json/Jsonb/Enum 列（算術のみ・
        // 等価/前方一致のみが受理形）への範囲比較はいずれも `22000`。
        _ => Err(unsupported_compare_column(column)),
    }
}

/// [`crate::numeric::NumericError`] を `wire_code` へ写像する（`sql::parser::
/// bind_numeric_literal` と同じ分類。エラーメッセージには列名のみを含める）。
fn numeric_literal_error(column: &str, e: crate::numeric::NumericError) -> SqlSurfaceError {
    match e {
        crate::numeric::NumericError::Malformed(detail) => {
            SqlSurfaceError::invalid_text_representation(format!("column {column:?}: {detail}"))
        }
        crate::numeric::NumericError::OutOfRange => SqlSurfaceError::numeric_out_of_range(format!(
            "column {column:?} numeric comparison literal out of range"
        )),
    }
}

/// リテラル長がアロケーション前の上限を超えないことを検証する（`54000`）。
fn check_literal_len(value: &str) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    if len > MAX_TEXT_FIELD_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "metadata filter literal length {len} exceeds limit {MAX_TEXT_FIELD_LEN}"
        )));
    }
    Ok(())
}

/// `pattern`（`LIKE` 句の右辺リテラル）を前方一致の prefix へ変換する。
///
/// SQL 表層の `LIKE` は Issue #914（SQL-24）以降 [`DeclarativeFilter::like`]／
/// [`parse_like_pattern`] を経由し、本関数は通らない（中間一致・後方一致・
/// `_` を受理するため）。本関数は Rust API 直接呼び出し
/// （[`DeclarativeFilter::starts_with`] 等）向けの前方一致限定パーサーとして
/// 公開 API のまま残す。
///
/// 受理する形状は「末尾がちょうど 1 つの `%` で、それ以外に `%`・`_`・`\` を
/// 含まず、prefix が非空」のみ（PG の `LIKE` 全体は実装せず前方一致だけに限定して
/// fail-closed に倒す）。以下はすべて `22000` で拒否する:
/// - 末尾に `%` が無い（`'abc'`）
/// - prefix が空（`'%'`）
/// - 中間・先頭に `%` を含む（`'a%b%'`・`'%abc'`）
/// - `_`（1 文字ワイルドカード）を含む
/// - `\`（エスケープ）を含む
pub fn parse_prefix_pattern(pattern: &str) -> Result<String, SqlSurfaceError> {
    let Some(prefix) = pattern.strip_suffix('%') else {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE pattern must end with exactly one '%' (prefix match only)",
        ));
    };
    if prefix.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE prefix must not be empty",
        ));
    }
    if prefix.contains(['%', '_', '\\']) {
        return Err(SqlSurfaceError::invalid_input(
            "LIKE pattern supports only a trailing '%' prefix match ('%', '_', '\\\\' elsewhere are not supported)",
        ));
    }
    Ok(prefix.to_string())
}

/// [`LikePattern`] を構成する 1 トークン。パターンをエスケープ解除しながら
/// 1 パス（`chars()`）で分解した中間表現で、[`LikePattern::matches`] が
/// この列に対して評価する。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LikeToken {
    /// リテラル 1 文字（エスケープ解除済み。`\%`・`\_`・`\\` を含む）。
    Char(char),
    /// `_`: ちょうど 1 **文字**（Unicode scalar）に一致する。
    Any,
    /// `%`: 空列を含む任意の文字列に一致する。連続する `%%` は構築時に 1 つへ
    /// 正規化する（[`parse_like_pattern`] 参照）。
    Star,
}

/// `LIKE` の一般形（中間一致・後方一致・`_` を含む）を表す、コンパイル済み
/// パターン（SQL-24／TASK-208、Issue #914）。`MetadataFilter::matches` から
/// [`Self::matches`] で評価する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LikePattern {
    tokens: Vec<LikeToken>,
}

impl LikePattern {
    /// `s`（対象列の値）がこのパターンに一致するか判定する。
    ///
    /// 貪欲法による古典的なワイルドカード照合アルゴリズム（`%` を跨ぐ再走査は
    /// 直近の `%` 位置へ戻るだけで、再帰・バックトラックの指数爆発は起きない）。
    /// 計算量は最悪 O(n·m)（n = `s` の文字数、m = パターンの文字数。m は
    /// [`MAX_LIKE_PATTERN_LEN`] で定数に抑えられる）。`s` の走査はバイト
    /// オフセット `ti`（`str::get()` でその位置から 1 文字だけ復号する）で
    /// 行い、`Vec<char>` へ事前展開しない（行ごとの評価コストを抑えるため）。
    /// 添字アクセス `[]` は使わず `get()` で明示的に処理する
    /// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
    pub fn matches(&self, s: &str) -> bool {
        let pat = &self.tokens;

        // `ti`／`resume_at` は `s` の**バイト**オフセット。`s.get(ti..)` の
        // 先頭 1 文字を都度復号することでマルチバイト文字を 1 文字として
        // 扱う（`char_indices` を回さず、現在位置だけを毎回 `get()` する）。
        let mut ti = 0usize;
        let mut pi = 0usize;
        let mut star_at: Option<usize> = None;
        let mut resume_at = 0usize;

        while let Some(current) = s.get(ti..).and_then(|rest| rest.chars().next()) {
            let advanced = match pat.get(pi) {
                Some(LikeToken::Char(c)) if *c == current => {
                    ti += current.len_utf8();
                    pi += 1;
                    true
                }
                Some(LikeToken::Any) => {
                    ti += current.len_utf8();
                    pi += 1;
                    true
                }
                Some(LikeToken::Star) => {
                    star_at = Some(pi);
                    resume_at = ti;
                    pi += 1;
                    true
                }
                _ => false,
            };
            if advanced {
                continue;
            }
            match star_at {
                Some(star_pi) => {
                    pi = star_pi + 1;
                    // 直近の `%` の再走査開始位置を 1 文字分だけ進める。
                    let step = s
                        .get(resume_at..)
                        .and_then(|rest| rest.chars().next())
                        .map(char::len_utf8)
                        .unwrap_or(1);
                    resume_at += step;
                    ti = resume_at;
                }
                None => return false,
            }
        }
        while matches!(pat.get(pi), Some(LikeToken::Star)) {
            pi += 1;
        }
        pi == pat.len()
    }
}

/// [`parse_like_pattern`] が返す、振り分け済みのコンパイル結果
/// （SQL-24／TASK-208、Issue #914）。索引利用を最大化するため、ワイルドカード
/// を含まない完全一致・純粋な前方一致（末尾 `%` のみ）は専用 variant へ、
/// それ以外の一般形（中間一致・後方一致・`_`）だけを [`Self::General`] に
/// 収める。呼び出し元（[`DeclarativeFilter::bind_impl`]）はこれを
/// `FilterOp::Equals`／`FilterOp::StartsWith`／`FilterOp::Like` へ写像する。
#[derive(Debug)]
pub enum CompiledLike {
    Exact(String),
    Prefix(String),
    General(LikePattern),
}

/// `pattern`（`LIKE` 句の右辺リテラル。生パターン・エスケープ解除前）を
/// コンパイルする（SQL-24／TASK-208、Issue #914。PostgreSQL 互換のワイルドカード
/// 意味論。詳細な契約は ADR `docs/design/like-wildcard-patterns.md` 参照）。
///
/// 意味論:
/// - `%`: 空列を含む任意の文字列に一致する（連続する `%%` は 1 つに正規化）。
/// - `_`: ちょうど 1 文字（Unicode scalar）に一致する。
/// - `\`: 既定のエスケープ文字。`\%`・`\_`・`\\` はそれぞれリテラルの
///   `%`・`_`・`\` として扱い、`\<その他>` はリテラル `<その他>` として扱う。
///   パターン末尾の単独 `\`（次の文字が無い）は `22000`。
/// - `ESCAPE` 句は未対応（構文層で受理しない。呼び出し元は本関数に到達しない）。
///
/// 長さ検証はアロケーション・パース**より前**に行う（`.claude/rules/
/// coding-rust.md`「untrusted 入力の扱い」）。[`MAX_LIKE_PATTERN_LEN`] 超は
/// `54000`。`unwrap`/`expect`/添字アクセス `[]` は使わない。
pub fn parse_like_pattern(pattern: &str) -> Result<CompiledLike, SqlSurfaceError> {
    if pattern.len() > MAX_LIKE_PATTERN_LEN {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "LIKE pattern length {} exceeds limit {MAX_LIKE_PATTERN_LEN}",
            pattern.len()
        )));
    }

    let mut tokens: Vec<LikeToken> = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(escaped) => tokens.push(LikeToken::Char(escaped)),
                None => {
                    return Err(SqlSurfaceError::invalid_input(
                        "LIKE pattern must not end with a trailing escape character '\\'",
                    ));
                }
            },
            '%' => {
                if !matches!(tokens.last(), Some(LikeToken::Star)) {
                    tokens.push(LikeToken::Star);
                }
            }
            '_' => tokens.push(LikeToken::Any),
            other => tokens.push(LikeToken::Char(other)),
        }
    }

    let has_any = tokens.iter().any(|t| matches!(t, LikeToken::Any));
    let star_count = tokens
        .iter()
        .filter(|t| matches!(t, LikeToken::Star))
        .count();

    if !has_any && star_count == 0 {
        let literal: String = tokens
            .iter()
            .filter_map(|t| match t {
                LikeToken::Char(c) => Some(*c),
                _ => None,
            })
            .collect();
        return Ok(CompiledLike::Exact(literal));
    }

    // 添字アクセス `[]` を使わず `split_last()` で末尾要素と残りを同時に取得する
    // （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」。`pattern` は
    // wire 経路由来の untrusted な SQL リテラル）。
    if !has_any && star_count == 1 {
        if let Some((LikeToken::Star, rest)) = tokens.split_last() {
            if !rest.is_empty() {
                let prefix: String = rest
                    .iter()
                    .filter_map(|t| match t {
                        LikeToken::Char(c) => Some(*c),
                        _ => None,
                    })
                    .collect();
                return Ok(CompiledLike::Prefix(prefix));
            }
        }
    }

    Ok(CompiledLike::General(LikePattern { tokens }))
}

/// 束縛済みのメタデータフィルタ 1 件（列インデックス解決済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFilter {
    column_index: usize,
    op: FilterOp,
}

impl MetadataFilter {
    /// スキーマ上の列インデックス（[`crate::row_codec::scan_scalar_columns`] が
    /// 返す `Vec` の添字と一致する）。
    pub fn column_index(&self) -> usize {
        self.column_index
    }

    /// フィルタの意味論。
    pub fn op(&self) -> &FilterOp {
        &self.op
    }

    /// 束縛済みの TEXT／ENUM 辞書等価の値集合から `InText` フィルタを組み立てる
    /// （Issue #1305。`sql::where_tree::BoundOrGroup::as_same_column_text_in` が
    /// 同じ列への等価 `OR` を 1 本の `IN` へ畳む際に使う）。
    ///
    /// 入力は束縛済み（長さ検査・ENUM ラベル検証済み）を前提とし、束縛時の `IN` と
    /// 同じく sort・dedup する（`InText` の `binary_search` 評価と、索引側の
    /// 辞書スロット昇順連結が前提とする不変条件）。空、または重複除去後に
    /// [`MAX_IN_LIST_ITEMS`] を超える場合は `None`（「畳まない」の意味でありエラー
    /// ではない。呼び出し元は従来どおり `OR` 群のまま残す＝fail-closed）。
    pub(crate) fn from_bound_text_in(
        column_index: usize,
        mut values: Vec<String>,
    ) -> Option<MetadataFilter> {
        values.sort();
        values.dedup();
        if values.is_empty() || values.len() > MAX_IN_LIST_ITEMS {
            return None;
        }
        Some(MetadataFilter {
            column_index,
            op: FilterOp::InText(values),
        })
    }

    /// 単体テスト用に束縛済みフィルタを直接組み立てる。
    #[cfg(test)]
    pub(crate) fn for_test(column_index: usize, op: FilterOp) -> MetadataFilter {
        MetadataFilter { column_index, op }
    }

    /// `value`（対象列の値。`None` は NULL）がこのフィルタに一致するかを PG 互換の
    /// 三値論理（`eval`）で判定し、`Some(true)` のときだけ真とする（SQL-24。
    /// TASK-208 ポインタ）。UNKNOWN（型不一致・NULL 経由の未確定）は不一致として
    /// 扱う（fail-closed。`NOT` による反転は [`FilterOp::eval`] 内で UNKNOWN の
    /// まま保たれるため、ここで `false` に丸めても `NOT` が誤って真に反転する
    /// ことはない）。
    pub fn matches(&self, value: Option<ScalarRef<'_>>) -> bool {
        self.eval(value) == Some(true)
    }

    /// `value` に対する三値論理での評価結果（`None` は UNKNOWN）。
    /// [`Self::matches`] の内部実装であり、`FilterOp::Not` が UNKNOWN を保った
    /// まま反転するために必要（`matches` の bool 版だけでは `NOT` 評価時に
    /// UNKNOWN と FALSE を区別できず、型不一致行が `NOT` 越しに誤って真になる
    /// ——fail-open——おそれがある）。
    pub(crate) fn eval(&self, value: Option<ScalarRef<'_>>) -> Option<bool> {
        self.op.eval(value)
    }
}

/// `count` 件のフィルタが [`MAX_METADATA_FILTERS`] を超えないことを検証する
/// （`54000`）。[`bind_all`] の件数検査本体を切り出したもので、`Vec` 確保・
/// 要素の複製より**前**に呼べる形にする（`.claude/rules/security.md`
/// 「不安全な設計｜無制限リソース確保（DoS）」対応）。
///
/// `pub`: `wire-server::http::query::filter`（NoSQL 表層の `filter` 配列。
/// Issue #761・TASK-175・NOSQL-7）が、JSON 配列要素を [`DeclarativeFilter`]
/// へ写像する**前**（`String` 複製・`Vec` 確保より前）に同じ上限を検査する
/// ために呼ぶ。`bind_all` と別々に上限を持たない単一情報源。
pub fn check_filter_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_METADATA_FILTERS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "metadata filter count {count} exceeds limit {MAX_METADATA_FILTERS}"
        )));
    }
    Ok(())
}

/// `filters` を `schema` へ一括束縛する。件数が [`MAX_METADATA_FILTERS`] を超える
/// 場合は `Vec` を確保する**前**に `54000` で拒否する。
pub fn bind_all(
    filters: &[DeclarativeFilter],
    schema: &TableSchema,
) -> Result<Vec<MetadataFilter>, SqlSurfaceError> {
    check_filter_count(filters.len())?;
    let mut bound = Vec::with_capacity(filters.len());
    for filter in filters {
        bound.push(filter.bind(schema)?);
    }
    Ok(bound)
}

/// [`bind_all`] の Prepared Describe 専用版（PR #1012 Cursor Bugbot 指摘対応。
/// Issue #935・WIRE-12・TASK-217）。`filters[i]` を束縛する際、
/// `skip_enum_label_validation[i]`（範囲外は `false` 扱い）が `true` の場合に
/// 限り ENUM 列の等価フィルタの語彙照合を省略する。呼び出し元
/// （`sql::parser::bind_where_predicates`）は、`sql::params::
/// order_by_distance_literal_is_param` と同じ設計で「そのフィルタの値が
/// `$n` に由来する固定ダミーかどうか」を並べたスライスを渡す。`filters` と
/// `skip_enum_label_validation` の対応は呼び出し元が構築順を揃えて保証する
/// 契約（本関数自身は対応関係を検証しない）。
pub(crate) fn bind_all_for_describe(
    filters: &[DeclarativeFilter],
    schema: &TableSchema,
    skip_enum_label_validation: &[bool],
) -> Result<Vec<MetadataFilter>, SqlSurfaceError> {
    check_filter_count(filters.len())?;
    let mut bound = Vec::with_capacity(filters.len());
    for (index, filter) in filters.iter().enumerate() {
        let skip = skip_enum_label_validation
            .get(index)
            .copied()
            .unwrap_or(false);
        bound.push(filter.bind_impl(schema, skip)?);
    }
    Ok(bound)
}

/// `scanned`（`row_codec::scan_scalar_columns` が返す列値。添字は列インデックス）に
/// 対して `filters` を全件 AND 評価する。範囲外インデックスは列値が NULL の場合
/// （`Some(None)`）と区別し、常に不一致として扱う（fail-closed。`scanned` は
/// 投影・フィルタが必要とする列だけを保持する構造のため、束縛時に検証済みの
/// 列インデックスでも呼び出し元の保持方針次第では範囲外になり得る）。
/// [`FilterOp::IsNull`] 導入（SQL-24。TASK-208 ポインタ）により、範囲外を NULL と
/// 同一視すると「値を読めていない」列が誤って `IS NULL` に一致してしまう
/// （fail-open）ため、`Option<Option<ScalarRef>>::flatten` は使わずここで明示的に
/// 分岐する。
pub fn matches_all(filters: &[MetadataFilter], scanned: &[Option<ScalarRef<'_>>]) -> bool {
    filters.iter().all(|f| match scanned.get(f.column_index) {
        // 型不一致（`TEXT` フィルタに `Bool`／`Real`／`Double` 値、`BoolEquals` に
        // `Text` 値等）は `bind` が列型で事前に排除している契約だが、
        // `MetadataFilter::matches` 側で防御的に UNKNOWN（不一致）へ落とす
        // （F10: TEXT 系フィルタに対する REAL/DOUBLE も同様に「値なし」と同じ
        // 扱いになる）。
        Some(value) => f.matches(*value),
        None => false,
    })
}

#[cfg(test)]
mod tests {
    /// 壊れた格納値（JSON 要素が JSON として読めない）への配列等価・`IN` は UNKNOWN
    /// （`None`）で、`NOT` 越しにも反転しない（Issue #1357 D4・fail-closed）。
    #[test]
    fn array_equality_on_corrupt_stored_json_element_is_unknown() {
        use crate::catalog::ArrayElemType;
        use crate::row_codec::{ArrayRef, ArrayValue, ScalarRef};

        let key_value = ArrayValue::Json(vec![Some(r#"{"a":1}"#.to_string())]);
        let key = ArrayKey::from_value(&key_value).expect("key");
        // 長さ前置（4 バイト LE）＋ JSON として不正な本文。
        let mut corrupt = 4u32.to_le_bytes().to_vec();
        corrupt.extend_from_slice(b"{bad");
        let row = ArrayRef::from_owned(ArrayElemType::Json, 1, 0, &corrupt);
        let eq = FilterOp::ArrayEquals(key.clone());
        assert_eq!(eq.eval(Some(ScalarRef::Array(row))), None);
        assert_eq!(
            FilterOp::Not(Box::new(eq)).eval(Some(ScalarRef::Array(row))),
            None
        );
        let in_op = FilterOp::InArray(vec![key]);
        assert_eq!(in_op.eval(Some(ScalarRef::Array(row))), None);
    }

    #[test]
    fn from_bound_text_in_sorts_dedups_and_bounds() {
        let f = MetadataFilter::from_bound_text_in(3, vec!["b".into(), "a".into(), "a".into()])
            .unwrap();
        assert_eq!(f.column_index(), 3);
        assert!(matches!(f.op(), FilterOp::InText(v) if v == &["a".to_string(), "b".to_string()]));
        assert!(MetadataFilter::from_bound_text_in(0, vec![]).is_none());
        let n = |k: usize| (0..k).map(|i| format!("v{i:04}")).collect::<Vec<_>>();
        assert!(MetadataFilter::from_bound_text_in(0, n(MAX_IN_LIST_ITEMS)).is_some());
        assert!(MetadataFilter::from_bound_text_in(0, n(MAX_IN_LIST_ITEMS + 1)).is_none());
    }

    use super::*;
    use crate::catalog::ColumnDef;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("kind", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        )
    }

    /// TABLE-13・TASK-199、Issue #891: レーン B（範囲比較）が対象とする
    /// 5 型を 1 列ずつ持つスキーマ。
    fn typed_compare_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("at", ColumnType::Timestamp, true),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("ext_id", ColumnType::Uuid, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    #[test]
    fn equality_matches_and_mismatches() {
        let f = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("code"))));
        assert!(!f.matches(Some(ScalarRef::Text("docs"))));
    }

    #[test]
    fn prefix_matches_and_mismatches() {
        let f = DeclarativeFilter::starts_with("path", "src/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("src/lib.rs"))));
        assert!(!f.matches(Some(ScalarRef::Text("lib.rs"))));
    }

    #[test]
    fn null_never_matches() {
        let eq = DeclarativeFilter::equals("tag", "x")
            .bind(&schema())
            .unwrap();
        let pre = DeclarativeFilter::starts_with("tag", "x")
            .bind(&schema())
            .unwrap();
        assert!(!eq.matches(None));
        assert!(!pre.matches(None));
    }

    #[test]
    fn empty_prefix_is_rejected() {
        let err = DeclarativeFilter::starts_with("path", "")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // PR #1012 Cursor Bugbot 指摘の回帰: `bind_all_for_describe` は
    // `skip_enum_label_validation[i]` が `true` の位置に限り ENUM 列の等価
    // フィルタの語彙照合を省略し、それ以外（範囲外含む）は従来どおり
    // `bind`（`bind_impl(.., false)`）と同一の検証を行う。
    #[test]
    fn bind_all_for_describe_skips_enum_validation_only_at_flagged_positions() {
        use crate::storage::Storage;
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("declarative-filter-bind-all-for-describe");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let mood = storage
            .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
            .expect("create enum type");
        let schema_with_enum = TableSchema::new(
            "docs",
            vec![ColumnDef::new("mood", ColumnType::Enum(mood), true)],
        );

        // flags[0] = true（ダミー値扱い）: 語彙外ラベルでも束縛が成功する。
        let filters = [DeclarativeFilter::equals("mood", "not-a-real-mood")];
        let bound = bind_all_for_describe(&filters, &schema_with_enum, &[true])
            .expect("skip_enum_label_validation=true must accept an out-of-vocabulary label");
        assert_eq!(bound.len(), 1);

        // flags[0] = false（実値扱い）: 従来どおり `22P02` で拒否される。
        let err = bind_all_for_describe(&filters, &schema_with_enum, &[false])
            .expect_err("skip_enum_label_validation=false must reject the same invalid label");
        assert_eq!(err.wire_code(), "22P02");

        // flags が短い（対応する要素が無い）場合は `false` 扱い（安全側）。
        let err_default = bind_all_for_describe(&filters, &schema_with_enum, &[])
            .expect_err("missing flag entries must default to full validation");
        assert_eq!(err_default.wire_code(), "22P02");

        // 妥当なラベルは `skip_enum_label_validation` の値に関係なく常に成功する。
        let valid_filters = [DeclarativeFilter::equals("mood", "happy")];
        assert!(bind_all_for_describe(&valid_filters, &schema_with_enum, &[true]).is_ok());
        assert!(bind_all_for_describe(&valid_filters, &schema_with_enum, &[false]).is_ok());
    }

    #[test]
    fn parse_prefix_pattern_accepts_trailing_percent_only() {
        assert_eq!(parse_prefix_pattern("src/%").unwrap(), "src/");
    }

    #[test]
    fn parse_prefix_pattern_rejects_missing_trailing_percent() {
        assert_eq!(
            parse_prefix_pattern("abc").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_empty_prefix() {
        assert_eq!(parse_prefix_pattern("%").unwrap_err().wire_code(), "22000");
    }

    #[test]
    fn parse_prefix_pattern_rejects_middle_percent() {
        assert_eq!(
            parse_prefix_pattern("a%b%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_underscore() {
        assert_eq!(
            parse_prefix_pattern("a_%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_backslash() {
        assert_eq!(
            parse_prefix_pattern("a\\%").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_prefix_pattern_rejects_leading_percent_only_form() {
        assert_eq!(
            parse_prefix_pattern("%abc").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn bind_rejects_vector_column() {
        let err = DeclarativeFilter::equals("embedding", "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_rejects_unknown_column() {
        let err = DeclarativeFilter::equals("nope", "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn bind_all_rejects_over_limit_count_before_allocating() {
        let filters: Vec<DeclarativeFilter> = (0..=MAX_METADATA_FILTERS)
            .map(|i| DeclarativeFilter::equals("kind", i.to_string()))
            .collect();
        let err = bind_all(&filters, &schema()).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn in_list_rejects_over_limit_element_count_before_allocating() {
        // PR #1118 codex-review P1 指摘: `check_predicate_limits`
        // （`sql::declarative_predicate`）は葉の総数しか数えず、`in_list` の
        // 要素数自体は検査しないため、SQL テキスト（`sql::allowlist::
        // parse_in_list_body`）を経由しない engine 公開 API の直接呼び出しでは
        // `MAX_IN_LIST_ITEMS` 超の `IN` が無検査で束縛され得た。`bind`（本モジュール
        // 側の唯一の共通防御点）がこの上限を検査することを確認する。
        let values = (0..=MAX_IN_LIST_ITEMS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>();
        let filter = DeclarativeFilter::in_list("kind", values);
        let err = filter.bind(&schema()).unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn check_filter_count_accepts_at_limit_and_rejects_over_limit() {
        assert!(check_filter_count(MAX_METADATA_FILTERS).is_ok());
        assert_eq!(
            check_filter_count(MAX_METADATA_FILTERS + 1)
                .unwrap_err()
                .wire_code(),
            "54000"
        );
    }

    #[test]
    fn multibyte_prefix_is_boundary_safe() {
        let f = DeclarativeFilter::starts_with("path", "日本語/")
            .bind(&schema())
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Text("日本語/doc.md"))));
        assert!(!f.matches(Some(ScalarRef::Text("語/doc.md"))));
    }

    #[test]
    fn matches_all_out_of_range_index_is_mismatch() {
        // 束縛済みフィルタの列インデックスが `scanned` の長さを超える異常系
        // （呼び出し元の保持方針の齟齬）でも fail-closed に不一致とする。
        let f = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        assert!(!matches_all(&[f], &[]));
    }

    // --- TABLE-13・TASK-199、Issue #891: レーン B（範囲比較）------------------

    #[test]
    fn typed_compare_date_equality_and_range() {
        let schema = typed_compare_schema();
        let eq = DeclarativeFilter::compare("day", CompareOp::Eq, "2024-01-01")
            .bind(&schema)
            .expect("bind DATE equality");
        assert!(eq.matches(Some(ScalarRef::Date(19723))));
        assert!(!eq.matches(Some(ScalarRef::Date(19724))));
        assert!(!eq.matches(None));

        let gt = DeclarativeFilter::compare("day", CompareOp::Gt, "2024-01-01")
            .bind(&schema)
            .expect("bind DATE range");
        assert!(gt.matches(Some(ScalarRef::Date(19724))));
        assert!(!gt.matches(Some(ScalarRef::Date(19723))));
    }

    #[test]
    fn typed_compare_timestamp_range() {
        let schema = typed_compare_schema();
        let le = DeclarativeFilter::compare("at", CompareOp::Le, "1970-01-01 00:00:01")
            .bind(&schema)
            .expect("bind TIMESTAMP range");
        assert!(le.matches(Some(ScalarRef::Timestamp(1_000_000))));
        assert!(le.matches(Some(ScalarRef::Timestamp(0))));
        assert!(!le.matches(Some(ScalarRef::Timestamp(1_000_001))));
    }

    #[test]
    fn typed_compare_numeric_does_not_round_to_column_scale() {
        // 列は NUMERIC(10, 2) だが、リテラル自身の scale（3 桁）をそのまま
        // 保持して正確に比較する（列の scale へ丸めると `1.005` が `1.01` に
        // 化けてしまい範囲比較の意味が変わる）。
        let schema = typed_compare_schema();
        let gt = DeclarativeFilter::compare("price", CompareOp::Gt, "1.005")
            .bind(&schema)
            .expect("bind NUMERIC range without rounding to column scale");
        let just_below = Decimal::from_parts(1004, 3).expect("1.004");
        let equal = Decimal::from_parts(1005, 3).expect("1.005");
        let just_above = Decimal::from_parts(1006, 3).expect("1.006");
        assert!(!gt.matches(Some(ScalarRef::Numeric(just_below))));
        assert!(!gt.matches(Some(ScalarRef::Numeric(equal))));
        assert!(gt.matches(Some(ScalarRef::Numeric(just_above))));

        // 列 scale（2 桁）で丸めた `1.01` と比較した場合、`1.005` は境界上に
        // なるが、丸めない正確な比較では `1.005 < 1.01` の関係を保つ。
        let rounded_to_column_scale = Decimal::from_parts(101, 2).expect("1.01");
        assert!(gt.matches(Some(ScalarRef::Numeric(rounded_to_column_scale))));
    }

    #[test]
    fn typed_compare_numeric_bare_number_literal() {
        let schema = typed_compare_schema();
        let ge = DeclarativeFilter::compare_numeric_literal("price", CompareOp::Ge, "2.5")
            .bind(&schema)
            .expect("bind NUMERIC bare-number-literal range");
        assert!(ge.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(250, 2).expect("2.50")
        ))));
        assert!(!ge.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(249, 2).expect("2.49")
        ))));
    }

    // Issue #1358: 指数表記の比較リテラルも丸めない正確な scale で束縛される
    // （`1e-3` が scale 0 の `0` に化けない）。
    #[test]
    fn typed_compare_numeric_exponent_literal_keeps_exact_scale() {
        let schema = typed_compare_schema();
        let gt = DeclarativeFilter::compare_numeric_literal("price", CompareOp::Gt, "1e-3")
            .bind(&schema)
            .expect("bind NUMERIC exponent literal");
        assert!(!gt.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(0, 2).expect("0.00")
        ))));
        assert!(gt.matches(Some(ScalarRef::Numeric(
            Decimal::from_parts(1, 2).expect("0.01")
        ))));
    }

    #[test]
    fn typed_compare_uuid_range_uses_byte_order() {
        let schema = typed_compare_schema();
        let gt = DeclarativeFilter::compare(
            "ext_id",
            CompareOp::Gt,
            "00000000-0000-0000-0000-000000000000",
        )
        .bind(&schema)
        .expect("bind UUID range");
        assert!(
            gt.matches(Some(ScalarRef::Uuid(crate::uuid::Uuid::from_bytes(
                [0xff; 16]
            ))))
        );
        assert!(
            !gt.matches(Some(ScalarRef::Uuid(crate::uuid::Uuid::from_bytes(
                [0x00; 16]
            ))))
        );
    }

    #[test]
    fn typed_compare_bytea_range_uses_dictionary_order() {
        let schema = typed_compare_schema();
        let lt = DeclarativeFilter::compare("blob", CompareOp::Lt, "\\xff")
            .bind(&schema)
            .expect("bind BYTEA range");
        assert!(lt.matches(Some(ScalarRef::Bytes(&[0xde, 0xad]))));
        assert!(!lt.matches(Some(ScalarRef::Bytes(&[0xff]))));
    }

    #[test]
    fn typed_compare_bytea_eq_accepts_decoded_length_up_to_insert_limit() {
        // 復号後 `MAX_BYTEA_FIELD_LEN`（4 MiB）ちょうどの値は insert/update と
        // 同じ実効上限で `eq` フィルタも受理できることを固定する（PR #1038
        // レビュー指摘の回帰防止。是正前は hex テキスト長〔約 8 MiB〕を
        // `MAX_TEXT_FIELD_LEN`〔4 MiB〕で検査していたため、復号後 約 2 MiB
        // 超で誤って `54000` になっていた）。
        let schema = typed_compare_schema();
        let hex_body = "ab".repeat(crate::bytea::MAX_BYTEA_FIELD_LEN as usize);
        let literal = format!("\\x{hex_body}");
        let eq = DeclarativeFilter::compare("blob", CompareOp::Eq, literal)
            .bind(&schema)
            .expect("decoded length at MAX_BYTEA_FIELD_LEN must bind for eq filter");
        let decoded = vec![0xab_u8; crate::bytea::MAX_BYTEA_FIELD_LEN as usize];
        assert!(eq.matches(Some(ScalarRef::Bytes(&decoded))));
    }

    #[test]
    fn typed_compare_bytea_eq_rejects_decoded_length_over_insert_limit() {
        // 復号後 `MAX_BYTEA_FIELD_LEN` を 1 バイト超える値は従来どおり `54000`
        // で拒否する（`bind_bytea_literal`／`parse_hex_text` 自身の上限判定）。
        let schema = typed_compare_schema();
        let hex_body = "ab".repeat(crate::bytea::MAX_BYTEA_FIELD_LEN as usize + 1);
        let literal = format!("\\x{hex_body}");
        let err = DeclarativeFilter::compare("blob", CompareOp::Eq, literal)
            .bind(&schema)
            .unwrap_err();
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn typed_compare_rejects_type_mismatched_column() {
        // TEXT 列（`path` 相当。ここでは `typed_compare_schema` に無いので
        // `schema()` の `path` を使う）への範囲比較は非対応列として `22000`。
        let err = DeclarativeFilter::compare("path", CompareOp::Gt, "x")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn typed_compare_rejects_malformed_literal_per_column_type() {
        let schema = typed_compare_schema();
        assert_eq!(
            DeclarativeFilter::compare("day", CompareOp::Eq, "not-a-date")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22007"
        );
        assert_eq!(
            DeclarativeFilter::compare("ext_id", CompareOp::Eq, "not-a-uuid")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22P02"
        );
        assert_eq!(
            DeclarativeFilter::compare("blob", CompareOp::Eq, "not-hex")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22P02"
        );
    }

    #[test]
    fn typed_compare_describe_dummy_skip_accepts_placeholder_without_parsing() {
        // Prepared Describe（`$n` 由来のダミー文字列 `"0"`）専用の縮退経路。
        // `"0"` は DATE/UUID/BYTEA の文法として不正だが、
        // `skip_enum_label_validation = true` の位置では実際には解析せず
        // プレースホルダ値へ縮退するため束縛は成功する（ENUM の語彙照合
        // スキップと同じ一般化。詳細は `bind_impl` のドキュメント参照）。
        let schema = typed_compare_schema();
        for column in ["day", "at", "price", "ext_id", "blob"] {
            let filters = [DeclarativeFilter::compare(column, CompareOp::Eq, "0")];
            let bound = bind_all_for_describe(&filters, &schema, &[true])
                .unwrap_or_else(|e| panic!("describe dummy skip for {column:?} failed: {e:?}"));
            assert_eq!(bound.len(), 1);

            // flags[0] = false（実値扱い）: ダミー文字列 `"0"` は
            // DATE/TIMESTAMP/UUID/BYTEA の文法としては不正なため束縛が
            // 失敗する。NUMERIC だけは `"0"` 自体が正当な数値リテラルの
            // ため、実値扱いでも成功する（ダミー値かどうかで結果が
            // 変わらない自明なケース）。
            let result = bind_all_for_describe(&filters, &schema, &[false]);
            if column == "price" {
                assert!(
                    result.is_ok(),
                    "NUMERIC column accepts \"0\" as a real literal too"
                );
            } else {
                let err = result.unwrap_err();
                assert!(
                    ["22000", "22P02", "22007"].contains(&err.wire_code()),
                    "column {column:?}: unexpected wire_code {:?}",
                    err.wire_code()
                );
            }
        }
    }

    // --- SQL-24（TASK-208 ポインタ）: IN / BETWEEN / IS [NOT] NULL / NOT ------

    #[test]
    fn in_text_matches_and_dedups_across_repeats() {
        let f = DeclarativeFilter::in_list(
            "kind",
            vec!["a".to_string(), "b".to_string(), "a".to_string()],
        )
        .bind(&schema())
        .unwrap();
        assert!(matches!(f.op(), FilterOp::InText(values) if values.len() == 2));
        assert!(f.matches(Some(ScalarRef::Text("a"))));
        assert!(f.matches(Some(ScalarRef::Text("b"))));
        assert!(!f.matches(Some(ScalarRef::Text("c"))));
        // NULL は UNKNOWN（不一致）。
        assert!(!f.matches(None));
    }

    #[test]
    fn in_list_rejects_empty_result_from_bind_all_for_unsupported_column() {
        // VECTOR 列（`embedding`）は IN の対象外（TEXT/ENUM/DATE/TIMESTAMP/
        // NUMERIC/UUID/BYTEA のいずれでもない）。
        let err = DeclarativeFilter::in_list("embedding", vec!["x".to_string()])
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn in_typed_matches_typed_compare_schema_columns() {
        let schema = typed_compare_schema();
        let f = DeclarativeFilter::in_list(
            "day",
            vec!["2024-01-01".to_string(), "2024-06-01".to_string()],
        )
        .bind(&schema)
        .unwrap();
        assert!(matches!(f.op(), FilterOp::InTyped(values) if values.len() == 2));
        assert!(f.matches(Some(ScalarRef::Date(19723))));
        assert!(!f.matches(Some(ScalarRef::Date(19724))));
        assert!(!f.matches(None));
    }

    #[test]
    fn between_matches_inclusive_bounds_and_null_is_unknown() {
        let schema = typed_compare_schema();
        let f = DeclarativeFilter::between("day", "2024-01-01", "2024-06-01")
            .bind(&schema)
            .unwrap();
        assert!(f.matches(Some(ScalarRef::Date(19723)))); // 2024-01-01（下限）
        assert!(f.matches(Some(ScalarRef::Date(19875)))); // 2024-06-01（上限）
        assert!(!f.matches(Some(ScalarRef::Date(19722))));
        assert!(!f.matches(Some(ScalarRef::Date(19876))));
        assert!(!f.matches(None));
    }

    #[test]
    fn between_low_greater_than_high_is_always_false_not_an_error() {
        let schema = typed_compare_schema();
        let f = DeclarativeFilter::between("day", "2024-06-01", "2024-01-01")
            .bind(&schema)
            .expect("low > high must bind successfully (PG semantics: always false)");
        assert!(!f.matches(Some(ScalarRef::Date(19723))));
        assert!(!f.matches(Some(ScalarRef::Date(19905))));
    }

    #[test]
    fn between_rejects_unsupported_column_type() {
        let err = DeclarativeFilter::between("path", "a", "z")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn is_null_and_is_not_null_partition_null_and_non_null() {
        let is_null = DeclarativeFilter::is_null("tag").bind(&schema()).unwrap();
        let is_not_null = DeclarativeFilter::is_not_null("tag")
            .bind(&schema())
            .unwrap();
        assert!(is_null.matches(None));
        assert!(!is_null.matches(Some(ScalarRef::Text("x"))));
        assert!(!is_not_null.matches(None));
        assert!(is_not_null.matches(Some(ScalarRef::Text("x"))));
    }

    #[test]
    fn is_null_rejects_vector_column() {
        let err = DeclarativeFilter::is_null("embedding")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
        let err = DeclarativeFilter::is_not_null("embedding")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    /// `ColumnType::Array`・`Json` を持つスキーマ（TABLE-14・TASK-198、Issue #888・
    /// #1193）。配列・JSON 列は `IS [NOT] NULL` に加えて等価・`IN` も束縛でき、
    /// `sql::exec::candidate_value_to_scalar_ref` は本物の `ArrayRef` を再構成して
    /// 評価に渡す（Issue #1193。空プレースホルダ前提は撤去済み）。
    fn array_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(
                        crate::catalog::ArrayType::new(crate::catalog::ArrayElemType::Text, 8)
                            .expect("array ty"),
                    ),
                    true,
                ),
                ColumnDef::new("doc", ColumnType::Jsonb, true),
            ],
        )
    }

    #[test]
    fn is_null_and_is_not_null_accept_array_column() {
        assert!(DeclarativeFilter::is_null("tags")
            .bind(&array_schema())
            .is_ok());
        assert!(DeclarativeFilter::is_not_null("tags")
            .bind(&array_schema())
            .is_ok());
    }

    #[test]
    fn array_column_accepts_equals_and_in_but_rejects_other_predicates() {
        let schema = array_schema();
        assert!(DeclarativeFilter::equals("tags", "{a,NULL}")
            .bind(&schema)
            .is_ok());
        assert!(
            DeclarativeFilter::in_list("tags", vec!["{a}".to_string(), "{}".to_string()])
                .bind(&schema)
                .is_ok()
        );
        // 形式違反は書き込みと同じ分類（22P02）。
        assert_eq!(
            DeclarativeFilter::equals("tags", "a,b")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22P02"
        );
        for filter in [
            DeclarativeFilter::starts_with("tags", "x"),
            DeclarativeFilter::like("tags", "%x%"),
            DeclarativeFilter::between("tags", "a", "b"),
            DeclarativeFilter::compare("tags", CompareOp::Gt, "x"),
        ] {
            assert_eq!(filter.bind(&schema).unwrap_err().wire_code(), "22000");
        }
    }

    fn array_ref_of(value: &crate::row_codec::ArrayValue) -> (Vec<u8>, u32, u8) {
        let mut payload = Vec::new();
        crate::row_codec::write_array_elements_payload(&mut payload, value).expect("payload");
        (payload, value.len() as u32, value.frame_flags())
    }

    #[test]
    fn array_equals_is_three_valued_and_null_elements_compare_equal() {
        use crate::row_codec::{ArrayRef, ArrayValue};
        let schema = array_schema();
        let filter = DeclarativeFilter::equals("tags", "{a,NULL}")
            .bind(&schema)
            .expect("bind");
        let same = ArrayValue::Text(vec![Some("a".to_string()), None]);
        let other = ArrayValue::Text(vec![Some("a".to_string()), Some("NULL".to_string())]);
        let (p1, c1, f1) = array_ref_of(&same);
        let (p2, c2, f2) = array_ref_of(&other);
        let elem = crate::catalog::ArrayElemType::Text;
        assert!(filter.matches(Some(ScalarRef::Array(ArrayRef::from_owned(
            elem, c1, f1, &p1
        )))));
        assert!(!filter.matches(Some(ScalarRef::Array(ArrayRef::from_owned(
            elem, c2, f2, &p2
        )))));
        // 列 NULL・型不一致は UNKNOWN（`NOT` 越しでも真にならない）。
        let negated = DeclarativeFilter::equals("tags", "{a,NULL}")
            .negate()
            .bind(&schema)
            .expect("bind");
        assert!(!filter.matches(None));
        assert!(!negated.matches(None));
        assert!(!negated.matches(Some(ScalarRef::Bytes(&[]))));
        // 空のプレースホルダ（旧 `Bytes(&[])`）は等価述語を満たさない。
        assert!(!filter.matches(Some(ScalarRef::Bytes(&[]))));
    }

    /// Prepared Describe（`$n` 由来のダミー値）では、配列・JSON 列の等価・`IN` は
    /// 右辺を解析せずプレースホルダへ縮退し、束縛に成功する（実値扱いなら不正）。
    #[test]
    fn array_and_json_describe_dummy_skip_accepts_placeholder_without_parsing() {
        let schema = array_schema();
        for column in ["tags", "doc"] {
            let filters = [DeclarativeFilter::equals(column, "0")];
            let bound = bind_all_for_describe(&filters, &schema, &[true])
                .unwrap_or_else(|e| panic!("describe dummy skip for {column:?} failed: {e:?}"));
            assert_eq!(bound.len(), 1);
            // 実値扱い: `"0"` は配列リテラルとして不正（JSON としては有効な数値）。
            let real = bind_all_for_describe(&filters, &schema, &[false]);
            if column == "tags" {
                assert_eq!(real.unwrap_err().wire_code(), "22P02");
            } else {
                assert!(real.is_ok());
            }
            let in_filters = [DeclarativeFilter::in_list(column, vec!["0".to_string()])];
            assert!(bind_all_for_describe(&in_filters, &schema, &[true]).is_ok());
        }
    }

    #[test]
    fn json_equals_uses_value_equality_normal_form() {
        let schema = array_schema();
        let filter = DeclarativeFilter::equals("doc", r#"{"b": 1.0, "a": [1, 2]}"#)
            .bind(&schema)
            .expect("bind");
        assert!(filter.matches(Some(ScalarRef::Json(r#"{"a":[1,2],"b":1}"#))));
        assert!(!filter.matches(Some(ScalarRef::Json(r#"{"a":[1,2],"b":2}"#))));
        assert!(!filter.matches(None));
        assert_eq!(
            DeclarativeFilter::equals("doc", "{not json")
                .bind(&schema)
                .unwrap_err()
                .wire_code(),
            "22P02"
        );
        let in_filter =
            DeclarativeFilter::in_list("doc", vec!["1".to_string(), r#""x""#.to_string()])
                .bind(&schema)
                .expect("bind");
        assert!(in_filter.matches(Some(ScalarRef::Json("1.0"))));
        assert!(in_filter.matches(Some(ScalarRef::Json(r#""x""#))));
        assert!(!in_filter.matches(Some(ScalarRef::Json("2"))));
    }

    #[test]
    fn negate_inverts_match_and_keeps_unknown_unknown() {
        let eq = DeclarativeFilter::equals("kind", "code")
            .bind(&schema())
            .unwrap();
        let not_eq = DeclarativeFilter::equals("kind", "code")
            .negate()
            .bind(&schema())
            .unwrap();
        assert!(eq.matches(Some(ScalarRef::Text("code"))));
        assert!(!not_eq.matches(Some(ScalarRef::Text("code"))));
        assert!(!eq.matches(Some(ScalarRef::Text("docs"))));
        assert!(not_eq.matches(Some(ScalarRef::Text("docs"))));
        // NULL: 両方とも UNKNOWN のまま（`NOT` で真に反転しない。fail-closed）。
        assert!(!eq.matches(None));
        assert!(!not_eq.matches(None));
    }

    #[test]
    fn negate_type_mismatch_stays_unknown_not_true() {
        // `BoolEquals` に `Text` 値を渡す型不一致（`bind` が事前に排除する
        // 契約だが、`eval` 自身が UNKNOWN を返すことを直接確認する）。
        let not_flag = DeclarativeFilter::bool_equals("kind", true)
            .negate()
            .bind(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("kind", ColumnType::Boolean, true)],
            ))
            .unwrap();
        assert!(!not_flag.matches(Some(ScalarRef::Text("not-a-bool"))));
    }

    #[test]
    fn matches_all_out_of_range_index_does_not_satisfy_is_null() {
        // 範囲外インデックス（列値を読めていない）は NULL と同一視しない
        // （fail-closed。`IS NULL` が誤って真になってはならない）。
        let f = DeclarativeFilter::is_null("tag").bind(&schema()).unwrap();
        assert!(!matches_all(&[f], &[]));
    }

    #[test]
    fn matches_all_out_of_range_index_does_not_satisfy_is_not_null() {
        let f = DeclarativeFilter::is_not_null("tag")
            .bind(&schema())
            .unwrap();
        assert!(!matches_all(&[f], &[]));
    }

    // --- SQL-24・TASK-208、Issue #914: LIKE の中間一致・後方一致・ワイルドカード ---

    #[test]
    fn parse_like_pattern_classifies_prefix_exact_and_general_forms() {
        assert!(matches!(
            parse_like_pattern("src/%").unwrap(),
            CompiledLike::Prefix(p) if p == "src/"
        ));
        assert!(matches!(
            parse_like_pattern("a\\_b%").unwrap(),
            CompiledLike::Prefix(p) if p == "a_b"
        ));
        assert!(matches!(
            parse_like_pattern("abc").unwrap(),
            CompiledLike::Exact(p) if p == "abc"
        ));
        assert!(matches!(
            parse_like_pattern("a\\%b").unwrap(),
            CompiledLike::Exact(p) if p == "a%b"
        ));
        for general in ["%", "%abc", "a%b", "a_c", "%mid%"] {
            assert!(
                matches!(
                    parse_like_pattern(general).unwrap(),
                    CompiledLike::General(_)
                ),
                "pattern {general:?} must classify as General"
            );
        }
    }

    #[test]
    fn parse_like_pattern_normalizes_consecutive_percent() {
        // `a%%` は `a%` と同じ意味（連続する `%` を 1 つへ正規化）で、末尾が
        // 単一の `%` になるため `Prefix` へ振り分けられる。
        assert!(matches!(
            parse_like_pattern("a%%").unwrap(),
            CompiledLike::Prefix(p) if p == "a"
        ));
    }

    #[test]
    fn parse_like_pattern_escape_semantics() {
        assert!(matches!(
            parse_like_pattern("100\\%").unwrap(),
            CompiledLike::Exact(p) if p == "100%"
        ));
        assert!(matches!(
            parse_like_pattern("a\\\\b").unwrap(),
            CompiledLike::Exact(p) if p == "a\\b"
        ));
        // `\<その他>` はリテラル `<その他>` として扱う。
        assert!(matches!(
            parse_like_pattern("a\\xb").unwrap(),
            CompiledLike::Exact(p) if p == "axb"
        ));
    }

    #[test]
    fn parse_like_pattern_rejects_trailing_backslash() {
        assert_eq!(
            parse_like_pattern("src\\").unwrap_err().wire_code(),
            "22000"
        );
    }

    #[test]
    fn parse_like_pattern_rejects_over_limit_length() {
        let at_limit = "a".repeat(MAX_LIKE_PATTERN_LEN);
        assert!(parse_like_pattern(&at_limit).is_ok());
        let over_limit = "a".repeat(MAX_LIKE_PATTERN_LEN + 1);
        assert_eq!(
            parse_like_pattern(&over_limit).unwrap_err().wire_code(),
            "54000"
        );
    }

    #[test]
    fn like_pattern_matches_suffix_middle_and_wildcard() {
        let suffix = match parse_like_pattern("%.rs").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(suffix.matches("lib.rs"));
        assert!(!suffix.matches("lib.rsx"));

        let middle = match parse_like_pattern("%/lib%").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(middle.matches("src/lib.rs"));
        assert!(!middle.matches("src/main.rs"));

        let single = match parse_like_pattern("src/_.rs").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(single.matches("src/a.rs"));
        assert!(!single.matches("src/ab.rs"));
        assert!(!single.matches("src/.rs"));
    }

    #[test]
    fn like_pattern_underscore_matches_one_unicode_char() {
        let pattern = match parse_like_pattern("日_語").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(pattern.matches("日本語"));
        assert!(!pattern.matches("日語"));
        assert!(!pattern.matches("日本本語"));
    }

    #[test]
    fn like_pattern_percent_alone_matches_all_non_empty_and_empty() {
        let pattern = match parse_like_pattern("%").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(pattern.matches(""));
        assert!(pattern.matches("anything"));
    }

    #[test]
    fn like_pattern_greedy_matching_is_correct() {
        // 貪欲法でも `%a%b%` と `xaxbx` のような複数候補がある形で正しく
        // 一致すること（バックトラックの正しさの固定）。
        let pattern = match parse_like_pattern("%a%b%").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(pattern.matches("xaxbx"));
        assert!(!pattern.matches("xbxax"));

        let overlap = match parse_like_pattern("ab%ba").unwrap() {
            CompiledLike::General(p) => p,
            other => panic!("expected General, got {other:?}"),
        };
        assert!(overlap.matches("ababa"));
        assert!(!overlap.matches("aba"));
    }

    #[test]
    fn like_pattern_brute_force_oracle_over_small_alphabet() {
        // `{a, b}` 上の全パターン（長さ 0..=3、`%`・`_` を含む）× 全値
        // （長さ 0..=4）を、再帰的な参照実装（オラクル）と突き合わせる
        // 小規模な網羅比較（貪欲法の正しさの追加固定）。
        fn oracle_matches(pattern: &[char], value: &[char]) -> bool {
            match pattern.split_first() {
                None => value.is_empty(),
                Some((&'%', rest)) => (0..=value.len()).any(|i| oracle_matches(rest, &value[i..])),
                Some((&'_', rest)) => !value.is_empty() && oracle_matches(rest, &value[1..]),
                Some((&c, rest)) => {
                    !value.is_empty() && value[0] == c && oracle_matches(rest, &value[1..])
                }
            }
        }

        let alphabet = ['a', 'b', '%', '_'];
        let values_alphabet = ['a', 'b'];

        fn combinations(alphabet: &[char], len: usize) -> Vec<Vec<char>> {
            if len == 0 {
                return vec![Vec::new()];
            }
            let mut out = Vec::new();
            for rest in combinations(alphabet, len - 1) {
                for &c in alphabet {
                    let mut v = vec![c];
                    v.extend(rest.iter().copied());
                    out.push(v);
                }
            }
            out
        }

        let mut patterns: Vec<Vec<char>> = vec![Vec::new()];
        for len in 1..=3 {
            patterns.extend(combinations(&alphabet, len));
        }
        let mut values: Vec<Vec<char>> = vec![Vec::new()];
        for len in 1..=4 {
            values.extend(combinations(&values_alphabet, len));
        }

        for pattern_chars in &patterns {
            let pattern_str: String = pattern_chars.iter().collect();
            let compiled = match parse_like_pattern(&pattern_str) {
                Ok(CompiledLike::General(p)) => p,
                Ok(CompiledLike::Exact(literal)) => LikePattern {
                    tokens: literal.chars().map(LikeToken::Char).collect(),
                },
                Ok(CompiledLike::Prefix(prefix)) => {
                    let mut tokens: Vec<LikeToken> = prefix.chars().map(LikeToken::Char).collect();
                    tokens.push(LikeToken::Star);
                    LikePattern { tokens }
                }
                Err(_) => continue,
            };
            for value_chars in &values {
                let value_str: String = value_chars.iter().collect();
                let expected = oracle_matches(pattern_chars, value_chars);
                assert_eq!(
                    compiled.matches(&value_str),
                    expected,
                    "pattern {pattern_str:?} value {value_str:?}"
                );
            }
        }
    }

    #[test]
    fn declarative_filter_like_binds_to_equals_starts_with_or_like() {
        let s = schema();
        let exact = DeclarativeFilter::like("kind", "code").bind(&s).unwrap();
        assert!(matches!(exact.op(), FilterOp::Equals(v) if v == "code"));

        let prefix = DeclarativeFilter::like("path", "src/%").bind(&s).unwrap();
        assert!(matches!(prefix.op(), FilterOp::StartsWith(v) if v == "src/"));

        let general = DeclarativeFilter::like("path", "%mid%").bind(&s).unwrap();
        assert!(matches!(general.op(), FilterOp::Like(_)));
        assert!(general.matches(Some(ScalarRef::Text("a/mid/b"))));
        assert!(!general.matches(Some(ScalarRef::Text("a/b"))));
    }

    #[test]
    fn declarative_filter_like_rejects_non_text_column() {
        let err = DeclarativeFilter::like("embedding", "%x%")
            .bind(&schema())
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn declarative_filter_like_null_never_matches() {
        let f = DeclarativeFilter::like("tag", "%x%")
            .bind(&schema())
            .unwrap();
        assert!(!f.matches(None));
    }
}
