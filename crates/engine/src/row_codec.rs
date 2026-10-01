//! カタログスキーマ駆動の行エンコーダー（TASK-86、対象ビヘイビア: TABLE-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-86・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `catalog.rs` の [`TableSchema`] を入力に取り、その列定義（列順・型・
//! nullable）に従って行データをバイト列へ/から変換する。`catalog.rs` の
//! モジュールコメントが「行エンコーダーの列対応・NULL 解決は TASK-86 の管轄」と
//! 明示している責務をここで実装し、`ALTER TABLE ADD COLUMN`（TABLE-5）で
//! 追加された列を持たない既存行を NULL として読む前提もここで扱う。
//!
//! `storage.rs` との関係: 本モジュールは `storage.rs` の行フォーマット v2
//! （`encode_row`/`decode_row`、tenant_id・visibility 同居の RLS 行フォーマット）を
//! 変更・置き換えしない。独立したバイトレイアウト（本モジュールローカルな v1）を持つ
//! 次世代コーデックとして追加し、`Storage` の行テーブルへの統合（テーブル帰属機構）は
//! 後続タスク（TASK-87・TASK-89・TASK-90 系）の管轄とする。`Visibility` 型のみ
//! `storage.rs` から再利用する（`to_byte`/`from_byte` の未知バイト拒否をそのまま活かす）。
//!
//! body 列の複製方針: 本タスクでは「行ストアの Text 列として単一保管し、別ストアへの
//! 複製・圧縮は行わない」と確定する（[`MAX_TEXT_FIELD_LEN`] で fail-closed に制限）。
//! 全文検索・UDF が本文参照を要求した時点で再検討する。

use std::fmt;

use crate::catalog::{ArrayElemType, ArrayType, ColumnType, TableSchema, MAX_ARRAY_ELEMENTS};
use crate::numeric::Decimal;
use crate::storage::Visibility;
use crate::uuid::Uuid;

/// 行フォーマットの先頭バイト。値の追加・変更は破壊的変更として扱い、この値を
/// 更新する。未知バージョンは fail-closed に拒否する（`storage.rs::ROW_FORMAT_VERSION`
/// と同じ方針。マイグレーションは提供しない）。
const ROW_CODEC_FORMAT_VERSION: u8 = 1;

/// ヘッダの `tenant_id` 長フィールド（`u8`）が表現できる上限。本モジュールの
/// ヘッダレイアウトはカタログスキーマとは独立に `tenant_id` を持つため、
/// `storage.rs::MAX_TENANT_ID_LEN`（`u16` 表現）とは別の実装ローカルな上限を持つ。
const MAX_TENANT_ID_LEN: u8 = u8::MAX;

/// `Text` 列 1 つあたりのバイト長上限。`storage.rs::MAX_METADATA_LEN` と同値方針
/// （data-model.md 2026-08-22 追記・CORE-15 方針のポインタ準拠）。検証通過後の
/// 長さのみをアロケーションに使う。
/// TASK-147（EXT-3）: `declarative_filter::DeclarativeFilter::bind` がメタデータ
/// フィルタのリテラル長上限検査（`54000`）にこの値を再利用するため `pub(crate)`。
pub(crate) const MAX_TEXT_FIELD_LEN: u32 = 4 * 1024 * 1024;

/// `Vector` 列の次元数上限。`storage.rs::MAX_EMBEDDING_DIM`（永続化層が扱える上限）と
/// 同値を維持する。片方だけの変更を防ぐため下部の const assert でコンパイル時に強制する。
const MAX_EMBEDDING_DIM: u32 = 65_536;

const _: () = assert!(
    MAX_EMBEDDING_DIM == crate::storage::MAX_EMBEDDING_DIM,
    "row_codec::MAX_EMBEDDING_DIM must stay in sync with storage::MAX_EMBEDDING_DIM"
);

/// [`encode_scalar_columns`] が複数 `TEXT` 列を連結して積む出力バッファ全体の
/// 累計バイト上限。列ごとの上限 [`MAX_TEXT_FIELD_LEN`]（4 MiB）は列数分の
/// 掛け算で数百 MiB〜GiB 規模まで届き得るため、それとは別に累計側でも
/// 確保前に上限検証する（security.md「不安全な設計｜無制限リソース確保（DoS）」・
/// codex-review 指摘・PR #989「小さい SET でも既存の大きな行に対し無制限確保が
/// 可能」対応）。値は `storage::MAX_METADATA_LEN`（最終的にこの出力を格納する
/// `RowInput::metadata` 側の上限）と同期させ、片方だけの変更を防ぐため下部の
/// const assert でコンパイル時に強制する。
pub(crate) const MAX_SCALAR_PAYLOAD_LEN: u32 = 4 * 1024 * 1024;

const _: () = assert!(
    MAX_SCALAR_PAYLOAD_LEN == crate::storage::MAX_METADATA_LEN,
    "row_codec::MAX_SCALAR_PAYLOAD_LEN must stay in sync with storage::MAX_METADATA_LEN"
);

// `JSON`／`JSONB` 列（Issue #889）は TEXT と同じ presence(1) + `u32 LE` 長(4) +
// 本体の枠を共有する。`json::MAX_JSON_FIELD_LEN` は「この列 1 個だけが対象行の
// スカラーペイロードを占める場合、フレーミング込みでも `MAX_SCALAR_PAYLOAD_LEN`
// に収まる」性質（PR #1014 レビュー指摘対応。`json.rs::MAX_JSON_FIELD_LEN` の
// ドキュメント参照）を持たせる必要があり、`MAX_TEXT_FIELD_LEN` と同値にすると
// フレーミング分だけ超過してしまうため意図的に一致させない。片方だけの変更で
// この性質が崩れるのを防ぐため、下記 2 条件をコンパイル時に強制する。
const _: () = assert!(
    crate::json::MAX_JSON_FIELD_LEN as u128 + SCALAR_TEXT_ENTRY_OVERHEAD as u128
        <= MAX_SCALAR_PAYLOAD_LEN as u128,
    "json::MAX_JSON_FIELD_LEN + SCALAR_TEXT_ENTRY_OVERHEAD must fit within \
     row_codec::MAX_SCALAR_PAYLOAD_LEN"
);

const _: () = assert!(
    crate::json::MAX_JSON_FIELD_LEN <= MAX_TEXT_FIELD_LEN as usize,
    "json::MAX_JSON_FIELD_LEN must not exceed row_codec::MAX_TEXT_FIELD_LEN"
);

/// `TEXT` 列 1 個分のフレーミングオーバーヘッド（presence タグ 1 バイト＋長さ
/// プレフィックス 4 バイト）。[`encode_scalar_columns`] の実エンコードと
/// [`tenant::update_row_columns_unchecked`]（UPDATE の SET 値の対象行探索より
/// 前の累計上限検証）が同じ計算式を共有するために公開する（片方だけの更新で
/// 事前検証と実エンコードが乖離すると、行の存在有無で異なる応答を返す既存
/// テナント境界漏えいの再発につながる）。
pub(crate) const SCALAR_TEXT_ENTRY_OVERHEAD: u32 = 5;

/// `TEXT` 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + 長さ(4) + 本文）を計算する。オーバーフロー時は `Err`。
pub(crate) fn scalar_text_entry_len(text_len: u32) -> Result<u32> {
    SCALAR_TEXT_ENTRY_OVERHEAD
        .checked_add(text_len)
        .ok_or_else(|| RowCodecError::Invalid("scalar payload entry length overflow".to_string()))
}

/// `INTEGER` 列 1 個分のフレーム込みバイト数（presence(1) + 本体(4)。固定幅・
/// 長さプレフィックスなし。Issue #881 D2）。`tenant::validate_set_assignments` の
/// 事前検証と実エンコードが同じ計算式を共有するため公開する（[`SCALAR_TEXT_ENTRY_OVERHEAD`]
/// と同じ理由）。
pub(crate) const SCALAR_INT4_ENTRY_LEN: u32 = 5;

/// `BIGINT` 列 1 個分のフレーム込みバイト数（presence(1) + 本体(8)。Issue #881 D2）。
pub(crate) const SCALAR_INT8_ENTRY_LEN: u32 = 9;

/// `REAL` 値 1 個をスカラーペイロードへ書き込む場合のフレーム込みバイト数
/// （presence(1) + LE ビット列(4)）。長さプレフィクスは持たない（列の型が
/// 固定長を決めるため、`TEXT` と異なり書き込む必要がない）。`tenant.rs` の
/// `validate_set_assignments` が事前検証で同じ値を使う（[`SCALAR_TEXT_ENTRY_OVERHEAD`]
/// と同じ理由。事前検証と実エンコードの乖離はテナント境界漏えいに直結する）。
pub(crate) const SCALAR_REAL_ENTRY_LEN: u32 = 5;

/// `DOUBLE PRECISION` 値 1 個をスカラーペイロードへ書き込む場合のフレーム込み
/// バイト数（presence(1) + LE ビット列(8)）。
pub(crate) const SCALAR_DOUBLE_ENTRY_LEN: u32 = 9;

/// 列値の有無を示すタグバイト。未知の値は fail-closed に拒否する（presence の
/// 黙殺フォールバックは NULL/値ありの取り違えに直結するため許容しない）。
const PRESENCE_NULL: u8 = 0x00;
const PRESENCE_VALUE: u8 = 0x01;

/// 行エンコーダー層の公開エラー型。`catalog.rs`/`storage.rs` と同じ流儀で、
/// 欠落・上限超過・未知値・型不一致・切り詰め検出をすべて `Invalid` に集約する
/// （fail-closed。既定値へのフォールバックは行わない）。
///
/// エラーメッセージにはフィールドの内容（`tenant_id` の値・`body` 本文等）を含めず、
/// 長さ・上限値のみを含める（.claude/rules/security.md「テナント境界」: エラー経由での
/// 情報漏えい防止）。
#[derive(Debug)]
pub enum RowCodecError {
    Invalid(String),
}

impl fmt::Display for RowCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowCodecError::Invalid(msg) => write!(f, "invalid row data: {msg}"),
        }
    }
}

impl std::error::Error for RowCodecError {}

pub type Result<T> = std::result::Result<T, RowCodecError>;

/// 1 列分の値。[`ColumnType`] に対応する（`Null` は nullable 列にのみ許容される）。
/// `Integer`／`BigInt`（Issue #881・TABLE-13・TASK-196）は幅ごとに variant を
/// 分けることで、encode 時に `ColumnType` との型一致検査だけで値域の再検査なしに
/// 取り違えを防げる設計にした（D1）。
/// `Real`／`Double`（TABLE-13・TASK-196）は常に有限値かつ `-0.0` を保持しない
/// 正規化済み値（`scalar_float::parse_real`／`parse_double`・`canonicalize_*`
/// 参照）を前提とし、encode 側は非有限値を fail-closed に拒否する。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    Vector(Vec<f32>),
    /// 符号付き 32 ビット整数（`ColumnType::Integer`）。
    Integer(i32),
    /// 符号付き 64 ビット整数（`ColumnType::BigInt`）。
    BigInt(i64),
    Real(f32),
    Double(f64),
    /// 真偽値列の値（TABLE-13・TASK-196、Issue #883）。NULL とはバイト列上も
    /// 別物になる（[`PRESENCE_NULL`] とは別に 1 バイトの値本体を持つ）。
    Bool(bool),
    /// 日付列の値（TABLE-13・TASK-197、Issue #884）。1970-01-01 起点の日数
    /// （`i32`）。範囲・暦妥当性は [`crate::datetime`] が検証済みであることを
    /// 前提とする。
    Date(i32),
    /// 日時列の値（TABLE-13・TASK-197、Issue #884）。1970-01-01 00:00:00
    /// 起点のマイクロ秒（`i64`、タイムゾーンなし）。
    Timestamp(i64),
    /// 配列列の値（TABLE-14・TASK-198、Issue #888）。要素型ごとに variant を
    /// 分けることで、同一列の要素がすべて同じ型であることを型で保証する
    /// （`ColumnType::Array` の `ArrayElemType` と対応させる）。
    Array(ArrayValue),
    /// 可変長バイナリ列の値（TABLE-13・TASK-197、Issue #886）。NULL・空バイト列
    /// （`Vec::new()`）はバイト列上も区別する。UTF-8 検証を行わない点のみ
    /// [`Value::Text`] と異なり、行バイト表現（presence + `u32` LE 長 + 本体）は
    /// 共有する。
    Bytes(Vec<u8>),
    /// `JSON`／`JSONB` 列共通の値表現（TABLE-14・TASK-198、Issue #889）。行バイト
    /// 表現は [`Value::Text`] と同じ枠（presence + `u32` LE 長 + UTF-8 本体）を
    /// 共有する。`JSON` 列は検証済みの入力テキストをそのまま、`JSONB` 列は
    /// 正規化再シリアライズ済みのテキストを保持する契約とし、区別は
    /// `ColumnType::Json`／`ColumnType::Jsonb` にのみ持たせる（値表現は共有）。
    Json(String),
    /// ENUM 列の値（TABLE-14・TASK-198、Issue #890）。ラベル文字列を TEXT と
    /// 同じフレームで格納する（序数格納は採らない。`ColumnType::Enum` の
    /// ドキュメント参照）。語彙検証は encode 時（[`encode_row`]）に行い、decode
    /// 時は検査しない（Issue #890 D3。`ALTER TYPE ... ADD VALUE` 前に書いた行を
    /// 将来にわたって読める契約を維持するため）。
    Enum(String),
    /// 十進固定小数列の値（TABLE-13〔検討中〕・TASK-197、Issue #885）。行には
    /// `unscaled`（`i128` LE 16 バイト）のみを持ち `scale` は持たない
    /// （カタログの列型 `ColumnType::Numeric { scale, .. }` が唯一の正）。
    Numeric(Decimal),
    /// 128bit 識別子列の値（TABLE-13〔検討中〕・TASK-197、Issue #887）。
    /// 内部表現・正規テキストは [`crate::uuid::Uuid`] 参照。
    Uuid(Uuid),
}

/// 配列列 1 個分の値（Issue #888・#1193）。各要素は `Option`（`None` が NULL 要素。
/// TABLE-14）。列自体の NULL は [`Value::Null`] と区別する。
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayValue {
    Text(Vec<Option<String>>),
    Bool(Vec<Option<bool>>),
    Integer(Vec<Option<i32>>),
    BigInt(Vec<Option<i64>>),
    Real(Vec<Option<f32>>),
    Double(Vec<Option<f64>>),
    Date(Vec<Option<i32>>),
    Timestamp(Vec<Option<i64>>),
    Uuid(Vec<Option<Uuid>>),
}

/// `ArrayValue` の全 variant に同じ式を適用する内部マクロ（要素型が増えたときの
/// 網羅漏れを 1 箇所に集約する）。
macro_rules! array_dispatch {
    ($value:expr, $items:ident => $body:expr) => {
        match $value {
            ArrayValue::Text($items) => $body,
            ArrayValue::Bool($items) => $body,
            ArrayValue::Integer($items) => $body,
            ArrayValue::BigInt($items) => $body,
            ArrayValue::Real($items) => $body,
            ArrayValue::Double($items) => $body,
            ArrayValue::Date($items) => $body,
            ArrayValue::Timestamp($items) => $body,
            ArrayValue::Uuid($items) => $body,
        }
    };
}

impl ArrayValue {
    /// この値の要素型（`ColumnType::Array` の `ArrayType::elem()` と突合する
    /// ために使う。`pub(crate)`: `tenant::validate_set_assignments` が UPDATE
    /// SET 値の事前検証で参照する）。
    pub(crate) fn elem(&self) -> ArrayElemType {
        match self {
            ArrayValue::Text(_) => ArrayElemType::Text,
            ArrayValue::Bool(_) => ArrayElemType::Bool,
            ArrayValue::Integer(_) => ArrayElemType::Integer,
            ArrayValue::BigInt(_) => ArrayElemType::BigInt,
            ArrayValue::Real(_) => ArrayElemType::Real,
            ArrayValue::Double(_) => ArrayElemType::Double,
            ArrayValue::Date(_) => ArrayElemType::Date,
            ArrayValue::Timestamp(_) => ArrayElemType::Timestamp,
            ArrayValue::Uuid(_) => ArrayElemType::Uuid,
        }
    }

    /// 要素数（`pub(crate)`: 用途は [`Self::elem`] と同じ）。
    pub(crate) fn len(&self) -> usize {
        array_dispatch!(self, v => v.len())
    }

    /// 1 個以上の NULL 要素を含むか（含むときだけフレーム flags が
    /// [`ARRAY_FLAG_HAS_NULLS`] になる正準性の根拠）。
    pub fn has_null(&self) -> bool {
        array_dispatch!(self, v => v.iter().any(|e| e.is_none()))
    }

    /// 要素ごとの NULL フラグ（要素数は `max_len <= 1024` で上限済み）。
    fn null_flags(&self) -> Vec<bool> {
        array_dispatch!(self, v => v.iter().map(|e| e.is_none()).collect())
    }

    /// 複製・保持に要するヒープ量の概算（複製予算・結果サイズ見積もり用。
    /// TEXT は本文長の合計に要素あたりの `Option<String>` 構造体分を加える。空文字列
    /// を大量に含む配列で予算消費が 0 のまま管理領域が積み上がらないようにする
    /// 保守的な見積もりで、`sql::returning`・`sql::cursor`・`sql::join`・`sql::set_op`
    /// が共有する）。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        match self {
            ArrayValue::Text(items) => {
                let payload: usize = items.iter().flatten().map(|s| s.len()).sum();
                payload.saturating_add(
                    items
                        .len()
                        .saturating_mul(std::mem::size_of::<Option<String>>()),
                )
            }
            ArrayValue::Bool(v) => v.len().saturating_mul(std::mem::size_of::<Option<bool>>()),
            ArrayValue::Integer(v) => v.len().saturating_mul(std::mem::size_of::<Option<i32>>()),
            ArrayValue::BigInt(v) => v.len().saturating_mul(std::mem::size_of::<Option<i64>>()),
            ArrayValue::Real(v) => v.len().saturating_mul(std::mem::size_of::<Option<f32>>()),
            ArrayValue::Double(v) => v.len().saturating_mul(std::mem::size_of::<Option<f64>>()),
            ArrayValue::Date(v) => v.len().saturating_mul(std::mem::size_of::<Option<i32>>()),
            ArrayValue::Timestamp(v) => v.len().saturating_mul(std::mem::size_of::<Option<i64>>()),
            ArrayValue::Uuid(v) => v.len().saturating_mul(std::mem::size_of::<Option<Uuid>>()),
        }
    }

    /// 集合演算の行キーなど、行バイトを介さない一意化用の正準表現
    /// `(要素型序数, flags, 要素列ペイロード)`。ペイロードは行バイトと同じ
    /// [`write_array_elements_payload`] の出力（NULL ビットマップを含む）で、値に
    /// 対して単射になる。`sql::set_op` が第 3 のエンコードを作らず共有する。
    pub(crate) fn canonical_parts(&self) -> Result<(u8, u8, Vec<u8>)> {
        let ordinal = match self {
            ArrayValue::Text(_) => 0,
            ArrayValue::Bool(_) => 1,
            ArrayValue::Integer(_) => 2,
            ArrayValue::BigInt(_) => 3,
            ArrayValue::Real(_) => 4,
            ArrayValue::Double(_) => 5,
            ArrayValue::Date(_) => 6,
            ArrayValue::Timestamp(_) => 7,
            ArrayValue::Uuid(_) => 8,
        };
        let mut payload = Vec::new();
        write_array_elements_payload(&mut payload, self)?;
        Ok((ordinal, self.frame_flags(), payload))
    }

    /// [`Self::canonical_parts`] のペイロード長（確保せずに算出する）。
    pub(crate) fn canonical_payload_len(&self) -> Result<usize> {
        Ok(array_elements_byte_len(self.elem(), self)? as usize)
    }

    /// エンコードされるフレーム flags（NULL 要素を含むときだけ
    /// [`ARRAY_FLAG_HAS_NULLS`]）。
    pub(crate) fn frame_flags(&self) -> u8 {
        if self.has_null() {
            ARRAY_FLAG_HAS_NULLS
        } else {
            ARRAY_FLAGS_NONE
        }
    }
}

/// スカラー列走査（[`scan_scalar_columns`] 系）の借用結果。TEXT・REAL・
/// DOUBLE PRECISION・BOOLEAN・ARRAY のいずれも返せるよう `Option<&str>` から
/// 型付き化した（Issue #883・D-b、Issue #888。`INTEGER`／`BIGINT`（Issue #881
/// D3）・`Real`／`Double` は固定長ペイロードなので値そのものを複製コストなしに
/// 保持できる）。`VECTOR` 列・実際の NULL 列は走査結果として `None` になる。
/// `#[non_exhaustive]` は付けない（Issue #880 D1 と同じ方針。型追加時に
/// コンパイラが全呼び出し元を列挙する）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScalarRef<'a> {
    Text(&'a str),
    Integer(i32),
    BigInt(i64),
    Real(f32),
    Double(f64),
    Bool(bool),
    /// 日付列（TABLE-13・TASK-197、Issue #884）。1970-01-01 起点の日数。
    Date(i32),
    /// 日時列（TABLE-13・TASK-197、Issue #884）。1970-01-01 00:00:00 起点の
    /// マイクロ秒（タイムゾーンなし）。
    Timestamp(i64),
    /// 配列列の借用結果（Issue #888）。構造・UTF-8・要素上限はすべて走査時に
    /// 検証済みで、[`ArrayRef::to_value`] は追加のエラー処理なしに複製できる。
    Array(ArrayRef<'a>),
    /// `BYTEA` 列の借用結果（Issue #886）。TEXT 前提の消費側（[`as_text`]）へは
    /// 流入させない（fail-closed）。
    ///
    /// [`as_text`]: ScalarRef::as_text
    Bytes(&'a [u8]),
    /// `JSON`／`JSONB` 列の借用結果（Issue #889）。TEXT 前提の消費側
    /// （[`as_text`]。等価/前方一致フィルタ・二次索引・hybrid 本文・GROUP BY
    /// キー等）へは流入させない（fail-closed。TABLE-14 は WHERE 等価・GROUP BY
    /// キーを JSON 列の対象外とする）。
    ///
    /// [`as_text`]: ScalarRef::as_text
    Json(&'a str),
    /// `ENUM` 列の借用結果（Issue #890）。TEXT と行バイト表現を共有するが、
    /// TEXT 前提の消費側（[`as_text`]）へは流入させない（`Bytea` と同じ方針）。
    /// 等価比較・二次索引の辞書化は [`as_dictionary_text`] を経由する。
    ///
    /// [`as_text`]: ScalarRef::as_text
    /// [`as_dictionary_text`]: ScalarRef::as_dictionary_text
    Enum(&'a str),
    /// `NUMERIC` 列の借用結果（TABLE-13〔検討中〕・TASK-197、Issue #885）。
    Numeric(Decimal),
    /// `UUID` 列の借用結果（TABLE-13〔検討中〕・TASK-197、Issue #887）。
    Uuid(Uuid),
}

impl<'a> ScalarRef<'a> {
    /// TEXT 前提の既存消費側（等価/前方一致フィルタ・二次索引・hybrid 本文・
    /// GROUP BY キー等）が `Integer`／`BigInt`／`Real`／`Double`／`Bool`／
    /// `Date`／`Timestamp`／`Array`／`Bytes`／`Json`／`Enum` を取り違えて TEXT
    /// として扱わないよう、`Text` 以外は `None` を返す（fail-closed。呼び出し
    /// 元はスキーマ型で事前に対象外の列を除外するか、`None` を型不一致として
    /// 拒否する）。
    pub fn as_text(&self) -> Option<&'a str> {
        match self {
            ScalarRef::Text(s) => Some(s),
            ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ScalarRef::Bool(b) => Some(*b),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    /// `DATE` 列の内部表現（1970-01-01 起点の日数）。`Date` 以外は `None`
    /// （[`Self::as_text`] と同じ fail-closed 方針。Issue #884）。
    pub fn as_date(&self) -> Option<i32> {
        match self {
            ScalarRef::Date(d) => Some(*d),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    /// `TIMESTAMP` 列の内部表現（1970-01-01 00:00:00 起点のマイクロ秒）。
    /// `Timestamp` 以外は `None`（同上、Issue #884）。
    pub fn as_timestamp(&self) -> Option<i64> {
        match self {
            ScalarRef::Timestamp(t) => Some(*t),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    /// NUMERIC 前提の消費側（TABLE-13〔検討中〕・TASK-197、Issue #885）が
    /// `Text`/`Integer`/`BigInt`/`Bool`/`Array`/`Bytes`/`Json`/`Enum` を
    /// 取り違えないよう、`Numeric` 以外は `None` を返す（fail-closed。
    /// [`as_text`]/[`as_bool`] と同方針）。
    pub fn as_numeric(&self) -> Option<Decimal> {
        match self {
            ScalarRef::Numeric(d) => Some(*d),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    /// UUID 前提の消費側（TABLE-13〔検討中〕・TASK-197、Issue #887）が
    /// `Text`/`Integer`/`BigInt`/`Bool`/`Array`/`Bytes`/`Json`/`Enum`/`Numeric`
    /// を取り違えないよう、`Uuid` 以外は `None` を返す（fail-closed。
    /// [`as_numeric`] と同方針）。
    ///
    /// [`as_numeric`]: ScalarRef::as_numeric
    pub fn as_uuid(&self) -> Option<crate::uuid::Uuid> {
        match self {
            ScalarRef::Uuid(u) => Some(*u),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_) => None,
        }
    }

    /// `BYTEA` 前提の消費側（TABLE-13・TASK-197、Issue #886。
    /// `declarative_filter` の範囲比較〔TASK-199、Issue #891〕から呼ばれる）が
    /// `Text`/`Integer`/`BigInt`/`Bool`/`Array`/`Json`/`Enum`/`Numeric`/`Uuid`
    /// を取り違えないよう、`Bytes` 以外は `None` を返す（fail-closed。
    /// [`as_numeric`]/[`as_uuid`] と同方針）。
    ///
    /// [`as_numeric`]: ScalarRef::as_numeric
    /// [`as_uuid`]: ScalarRef::as_uuid
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            ScalarRef::Bytes(b) => Some(b),
            ScalarRef::Text(_)
            | ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Json(_)
            | ScalarRef::Enum(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }

    /// `Text`／`Enum` のみを許す辞書化アクセサ（Issue #890。スカラー列二次索引
    /// 〔`sql::scalar_index::ScalarIndex`〕・`declarative_filter` の等価比較が
    /// ENUM 列を TEXT 列と同じ辞書表現で扱えるようにするための限定共有。
    /// `Integer`／`BigInt`／`Bool`／`Array`／`Bytes`／`Json` は対象外のまま
    /// `None`（TABLE-14 が定める述語の範囲を超えて ENUM／JSON を露出しない）。
    pub fn as_dictionary_text(&self) -> Option<&'a str> {
        match self {
            ScalarRef::Text(s) | ScalarRef::Enum(s) => Some(s),
            ScalarRef::Integer(_)
            | ScalarRef::BigInt(_)
            | ScalarRef::Real(_)
            | ScalarRef::Double(_)
            | ScalarRef::Bool(_)
            | ScalarRef::Date(_)
            | ScalarRef::Timestamp(_)
            | ScalarRef::Array(_)
            | ScalarRef::Bytes(_)
            | ScalarRef::Json(_)
            | ScalarRef::Numeric(_)
            | ScalarRef::Uuid(_) => None,
        }
    }
}

/// [`ScalarRef::Array`] の借用結果（Issue #888）。走査（[`scan_scalar_columns_validated`]）
/// 時点で構造・UTF-8・要素数上限を検証済みの `bytes`（要素列のみ。フレームヘッダは
/// 含まない）を保持し、`to_value` はそれを信頼して再デコードする（デコード失敗時も
/// `Err` を返す設計を維持し、`unwrap` 等で無条件成功を仮定しない）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrayRef<'a> {
    elem: ArrayElemType,
    count: u32,
    /// フレーム flags（`0x00` または [`ARRAY_FLAG_HAS_NULLS`]）。`bytes` の先頭に
    /// NULL ビットマップが付くかどうかを決める。
    flags: u8,
    bytes: &'a [u8],
}

impl<'a> ArrayRef<'a> {
    pub fn elem(&self) -> ArrayElemType {
        self.elem
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// フレーム flags（NULL 要素を含むとき `0x01`。Issue #1193）。
    pub fn flags(&self) -> u8 {
        self.flags
    }

    /// 要素列本文（フレームヘッダを含まない）のバイト長。呼び出し元が
    /// 複製予算を見積もる際、`count()`（要素数）だけでは TEXT 要素の実体
    /// バイト数（可変長）を捉えられないため、こちらを主に使う
    /// （`sql::exec::try_alloc_array_for_budget`／`sql::scan::try_alloc_array_for_budget`
    /// 参照。Issue #888 レビュー指摘対応）。
    pub fn payload_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// 借用結果を所有値へ複製する（[`decode_scalar_columns`] 等が使う）。
    pub fn to_value(&self) -> Result<ArrayValue> {
        decode_array_elements(self.elem, self.bytes, self.count, self.flags)
    }

    /// 要素列本文（フレームヘッダを含まない。走査時点で構造・UTF-8・要素数上限を
    /// 検証済み）への借用（`pub(crate)`。[`crate::constraint::push_canonical_component`]
    /// が UNIQUE 制約の正準キー（Issue #1073）を組み立てる際に使う）。
    ///
    /// エンコーダ（[`write_array_value`]）は要素順を保持し、flags を
    /// [`ARRAY_FLAGS_RESERVED`]（`0x00`）固定、TEXT 要素は長さ前置＋本文、
    /// BOOL 要素は 1 バイトのいずれも代替表現を持たない決定的な形式でのみ
    /// エンコードするため、`(elem, count, payload)` の組は値に対して単射になる
    /// （呼び出し元がこの単射性に依存する契約。エンコーダの決定性を崩す変更は
    /// 一意性判定の正しさに影響する）。
    pub(crate) fn payload(&self) -> &'a [u8] {
        self.bytes
    }

    /// 束縛済み `VALUES`（`Value::Array`）から一意キー計算専用のスクラッチ
    /// `ArrayRef` を組み立てる（`pub(crate)`: [`crate::constraint::
    /// unique_key_from_values`] が UPSERT の `ON CONFLICT` 対象キー（TABLE-16・
    /// Issue #1074）として使う）。`bytes` は呼び出し元が
    /// [`write_array_elements_payload`] で組み立てた、走査結果の借用
    /// （[`scan_scalar_columns_masked`] 等）と同一フォーマットのバッファへの
    /// 借用。永続化された行から走査した結果とは異なり構造検証を経ていないが、
    /// `Value::Array` は列挿入時の束縛（`bind_insert_row`）で既にスキーマ検証
    /// 済みの値であるため、ここでの再検証は行わない契約とする。
    pub(crate) fn from_owned(elem: ArrayElemType, count: u32, flags: u8, bytes: &'a [u8]) -> Self {
        ArrayRef {
            elem,
            count,
            flags,
            bytes,
        }
    }
}

/// BOOLEAN 値のバイト表現（presence タグに続く 1 バイト）。`0x00`/`0x01`
/// 以外は decode 側で fail-closed に拒否する（既定値へのフォールバックはしない。
/// TABLE-7）。
const BOOL_FALSE_BYTE: u8 = 0x00;
const BOOL_TRUE_BYTE: u8 = 0x01;

/// BOOLEAN 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + 値(1)）。[`encode_scalar_columns`] の実エンコードと
/// `tenant::validate_set_assignments` の事前累計検証が同じ値を共有する
/// （TEXT の [`SCALAR_TEXT_ENTRY_OVERHEAD`] と同じ理由）。
pub(crate) const SCALAR_BOOL_ENTRY_LEN: u32 = 2;

/// `DATE` 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + 値(4, LE i32)。Issue #884）。
pub(crate) const SCALAR_DATE_ENTRY_LEN: u32 = 5;

/// `TIMESTAMP` 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込み
/// バイト数（presence(1) + 値(8, LE i64)。Issue #884）。
pub(crate) const SCALAR_TIMESTAMP_ENTRY_LEN: u32 = 9;

/// 配列列のフレーム flags バイト（NULL 要素なし）。ペイロードは要素列のみ。
const ARRAY_FLAGS_NONE: u8 = 0x00;

/// 配列列のフレーム flags バイト（NULL 要素あり。Issue #1193・D-A3 拡張）。
/// ペイロード先頭に `ceil(count/8)` バイトの NULL ビットマップ（LSB first・1 が
/// NULL・余りビットは 0）が付き、その後ろに非 NULL 要素だけが並ぶ。NULL が 1 個も
/// 無い配列には付けない（正準性: 値に対してバイト表現が一意になる）。`0x00`／
/// `0x01` 以外は decode 側で fail-closed に拒否する。
pub(crate) const ARRAY_FLAG_HAS_NULLS: u8 = 0x01;

/// 配列要素 1 個の BOOL 値バイト表現（スカラー BOOLEAN 列と同じ規約を再利用）。
const ARRAY_BOOL_FALSE_BYTE: u8 = BOOL_FALSE_BYTE;
const ARRAY_BOOL_TRUE_BYTE: u8 = BOOL_TRUE_BYTE;

/// 配列列 1 個分の要素列（フレームヘッダを含まない本文のみ）のバイト長上限
/// （D-A3）。列ごとの [`MAX_TEXT_FIELD_LEN`] と同値を採用し、`TEXT` 列 1 個分の
/// 上限と揃える。
const MAX_ARRAY_PAYLOAD_LEN: u32 = MAX_TEXT_FIELD_LEN;

/// 固定長要素型の 1 要素あたりのバイト幅（`TEXT` は可変長なので `None`）。
fn array_fixed_width(elem: ArrayElemType) -> Option<usize> {
    match elem {
        ArrayElemType::Text => None,
        ArrayElemType::Bool => Some(1),
        ArrayElemType::Integer | ArrayElemType::Real | ArrayElemType::Date => Some(4),
        ArrayElemType::BigInt | ArrayElemType::Double | ArrayElemType::Timestamp => Some(8),
        ArrayElemType::Uuid => Some(16),
    }
}

/// 配列要素列（フレームヘッダを含まない本文。NULL ビットマップを含む）の
/// バイト長を計算する。オーバーフロー時は `Err`（[`scalar_text_entry_len`] と
/// 同じ方針）。
fn array_elements_byte_len(elem: ArrayElemType, value: &ArrayValue) -> Result<u32> {
    if value.elem() != elem {
        return Err(RowCodecError::Invalid(
            "array value element type does not match column element type".to_string(),
        ));
    }
    let bitmap_len = if value.has_null() {
        value.len().div_ceil(8)
    } else {
        0
    };
    let mut total: usize = bitmap_len;
    match value {
        ArrayValue::Text(items) => {
            for item in items.iter().flatten() {
                // 長さプレフィックス(4) + 本文。
                let entry = item.len().checked_add(4).ok_or_else(|| {
                    RowCodecError::Invalid("array element length overflow".to_string())
                })?;
                total = total.checked_add(entry).ok_or_else(|| {
                    RowCodecError::Invalid("array payload length overflow".to_string())
                })?;
            }
        }
        other => {
            let width = array_fixed_width(elem).ok_or_else(|| {
                RowCodecError::Invalid("array element type has no fixed width".to_string())
            })?;
            let present = array_dispatch!(other, v => v.iter().filter(|e| e.is_some()).count());
            let body = present.checked_mul(width).ok_or_else(|| {
                RowCodecError::Invalid("array payload length overflow".to_string())
            })?;
            total = total.checked_add(body).ok_or_else(|| {
                RowCodecError::Invalid("array payload length overflow".to_string())
            })?;
        }
    }
    u32::try_from(total)
        .map_err(|_| RowCodecError::Invalid("array payload length overflow".to_string()))
}

/// 配列列 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + flags(1) + 要素数(4) + ペイロード長(4) + 要素列。NULL 要素が
/// あればビットマップを含む）。事前検証
/// （[`crate::tenant::validate_set_assignments`]）と実エンコードが同じ計算式を
/// 共有するために公開する（[`scalar_text_entry_len`] と同じ理由）。
pub(crate) fn scalar_array_entry_len(elem: ArrayElemType, value: &ArrayValue) -> Result<u32> {
    let payload_len = array_elements_byte_len(elem, value)?;
    if payload_len > MAX_ARRAY_PAYLOAD_LEN {
        return Err(RowCodecError::Invalid(format!(
            "array payload length {payload_len} exceeds limit {MAX_ARRAY_PAYLOAD_LEN}"
        )));
    }
    // presence(1) + flags(1) + count(4) + payload_len(4)。
    10u32
        .checked_add(payload_len)
        .ok_or_else(|| RowCodecError::Invalid("array entry length overflow".to_string()))
}

/// [`Value::Array`] 1 個をバッファへ書き込む（presence タグは呼び出し元が別途
/// 積む）。`array_ty` の `elem`/`max_len` との不一致（要素型違い・要素数超過）は
/// `Err`（TABLE-14）。
fn write_array_value(buf: &mut Vec<u8>, array_ty: ArrayType, value: &ArrayValue) -> Result<()> {
    if value.elem() != array_ty.elem() {
        return Err(RowCodecError::Invalid(
            "array value element type does not match column definition".to_string(),
        ));
    }
    let count = u32::try_from(value.len())
        .map_err(|_| RowCodecError::Invalid("array element count too large".to_string()))?;
    if count > array_ty.max_len() || count > MAX_ARRAY_ELEMENTS {
        return Err(RowCodecError::Invalid(format!(
            "array element count {count} exceeds limit {}",
            array_ty.max_len().min(MAX_ARRAY_ELEMENTS)
        )));
    }
    let payload_len = array_elements_byte_len(array_ty.elem(), value)?;
    if payload_len > MAX_ARRAY_PAYLOAD_LEN {
        return Err(RowCodecError::Invalid(format!(
            "array payload length {payload_len} exceeds limit {MAX_ARRAY_PAYLOAD_LEN}"
        )));
    }
    buf.push(value.frame_flags());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    write_array_elements_payload(buf, value)
}

/// 配列要素列（フレームヘッダを含まない本文のみ。NULL 要素があれば先頭に
/// ビットマップ）を `buf` へ書き込む（[`write_array_value`] の本体部分を切り出した
/// もの。ヘッダ（flags・`count`・`payload_len`）の書き込みは呼び出し元の責務。
/// flags は [`ArrayValue::frame_flags`]）。`pub(crate)`:
/// [`crate::constraint::unique_key_from_values`] が UPSERT の `ON CONFLICT`
/// 対象キー（TABLE-16・Issue #1074）として ARRAY 列の束縛値から一意キー計算用の
/// スクラッチ `ArrayRef`（[`ArrayRef::from_owned`]）を組み立てる際、この要素書き込み
/// ロジックを独立に再実装せず共有するために公開する。REAL／DOUBLE は非有限値を
/// 拒否し `-0.0` を正規化、DATE／TIMESTAMP は値域を検証する（永続化バイト列の
/// 正準性を保つ多層防御）。
pub(crate) fn write_array_elements_payload(buf: &mut Vec<u8>, value: &ArrayValue) -> Result<()> {
    let nulls = value.null_flags();
    if nulls.iter().any(|b| *b) {
        let mut bitmap = vec![0u8; nulls.len().div_ceil(8)];
        for (i, is_null) in nulls.iter().enumerate() {
            if *is_null {
                if let Some(byte) = bitmap.get_mut(i / 8) {
                    *byte |= 1u8 << (i % 8);
                }
            }
        }
        buf.extend_from_slice(&bitmap);
    }
    match value {
        ArrayValue::Text(items) => {
            for item in items.iter().flatten() {
                let item_bytes = item.as_bytes();
                // array_elements_byte_len ですでに u32 化に成功しているため
                // ここでの try_from は失敗しない想定だが、untrusted 経路の多層
                // 防御として改めて検証する。
                let item_len = u32::try_from(item_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid("array text element too long".to_string())
                })?;
                buf.extend_from_slice(&item_len.to_le_bytes());
                buf.extend_from_slice(item_bytes);
            }
        }
        ArrayValue::Bool(items) => {
            for b in items.iter().flatten() {
                buf.push(if *b {
                    ARRAY_BOOL_TRUE_BYTE
                } else {
                    ARRAY_BOOL_FALSE_BYTE
                });
            }
        }
        ArrayValue::Integer(items) => {
            for v in items.iter().flatten() {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        ArrayValue::BigInt(items) => {
            for v in items.iter().flatten() {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        ArrayValue::Real(items) => {
            for v in items.iter().flatten() {
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(
                        "array REAL element must be finite".to_string(),
                    ));
                }
                buf.extend_from_slice(&crate::scalar_float::canonicalize_real(*v).to_le_bytes());
            }
        }
        ArrayValue::Double(items) => {
            for v in items.iter().flatten() {
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(
                        "array DOUBLE PRECISION element must be finite".to_string(),
                    ));
                }
                buf.extend_from_slice(&crate::scalar_float::canonicalize_double(*v).to_le_bytes());
            }
        }
        ArrayValue::Date(items) => {
            for v in items.iter().flatten() {
                if !crate::datetime::validate_date_days(*v) {
                    return Err(RowCodecError::Invalid(
                        "array DATE element out of range".to_string(),
                    ));
                }
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        ArrayValue::Timestamp(items) => {
            for v in items.iter().flatten() {
                if !crate::datetime::validate_timestamp_micros(*v) {
                    return Err(RowCodecError::Invalid(
                        "array TIMESTAMP element out of range".to_string(),
                    ));
                }
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        ArrayValue::Uuid(items) => {
            for v in items.iter().flatten() {
                buf.extend_from_slice(v.as_bytes());
            }
        }
    }
    Ok(())
}

/// 走査済みの [`ArrayRef`]（構造検証済み）をそのままフレームとして書き出す
/// （presence タグは呼び出し元が別途積む）。[`merge_encode_scalar_columns`] の
/// SET 対象でない列（既存値の再エンコード）が使う。`write_array_value` と違い
/// `ArrayType` との突合は行わない（`existing` はすでに検証済みの borrow のため）。
/// flags と NULL ビットマップ（`bytes` の先頭）はそのまま書き戻す。
fn write_array_ref(buf: &mut Vec<u8>, array_ref: &ArrayRef) -> Result<()> {
    let payload_len = u32::try_from(array_ref.bytes.len())
        .map_err(|_| RowCodecError::Invalid("array payload length overflow".to_string()))?;
    buf.push(array_ref.flags);
    buf.extend_from_slice(&array_ref.count.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(array_ref.bytes);
    Ok(())
}

/// 固定長 `N` バイトの要素を読み出す（長さ不一致は `Err`。添字アクセスを使わない）。
fn array_le_bytes<const N: usize>(chunk: &[u8]) -> Result<[u8; N]> {
    chunk
        .try_into()
        .map_err(|_| RowCodecError::Invalid("array element has unexpected width".to_string()))
}

/// フレーム flags に従い NULL ビットマップを分離する。戻り値は要素ごとの NULL
/// フラグ（長さ `count`）と、ビットマップを除いた要素列本文。非正準形
/// （`0x01` なのに NULL ビットが 1 個も無い・余りビットが 1・未知 flags）は
/// fail-closed に `Err`（等価判定・UNIQUE キーが依存する単射性を守る）。
fn split_array_null_bitmap(bytes: &[u8], count: usize, flags: u8) -> Result<(Vec<bool>, &[u8])> {
    if count > MAX_ARRAY_ELEMENTS as usize {
        return Err(RowCodecError::Invalid(
            "array element count exceeds limit".to_string(),
        ));
    }
    match flags {
        ARRAY_FLAGS_NONE => Ok((vec![false; count], bytes)),
        ARRAY_FLAG_HAS_NULLS => {
            let bitmap_len = count.div_ceil(8);
            let bitmap = bytes.get(..bitmap_len).ok_or_else(|| {
                RowCodecError::Invalid("array payload truncated at null bitmap".to_string())
            })?;
            let rest = bytes.get(bitmap_len..).ok_or_else(|| {
                RowCodecError::Invalid("array payload truncated after null bitmap".to_string())
            })?;
            let mut nulls = Vec::with_capacity(count);
            for i in 0..count {
                let byte = bitmap.get(i / 8).copied().ok_or_else(|| {
                    RowCodecError::Invalid("array null bitmap too short".to_string())
                })?;
                nulls.push((byte >> (i % 8)) & 1 == 1);
            }
            if !count.is_multiple_of(8) {
                let last = bitmap.last().copied().unwrap_or(0);
                if last >> (count % 8) != 0 {
                    return Err(RowCodecError::Invalid(
                        "array null bitmap has non-zero padding bits".to_string(),
                    ));
                }
            }
            if !nulls.iter().any(|b| *b) {
                return Err(RowCodecError::Invalid(
                    "array flags declare nulls but the bitmap has none".to_string(),
                ));
            }
            Ok((nulls, rest))
        }
        other => Err(RowCodecError::Invalid(format!(
            "unknown array flags byte: {other}"
        ))),
    }
}

/// 固定長要素の本文を `nulls` に従って展開する。本文長が「非 NULL 要素数 × 幅」
/// とちょうど一致しない場合（不足・余剰いずれも）は `Err`。
fn decode_fixed_array_elements<T>(
    rest: &[u8],
    nulls: &[bool],
    width: usize,
    mut convert: impl FnMut(&[u8]) -> Result<T>,
) -> Result<Vec<Option<T>>> {
    let present = nulls.iter().filter(|is_null| !**is_null).count();
    let expected = present
        .checked_mul(width)
        .ok_or_else(|| RowCodecError::Invalid("array payload length overflow".to_string()))?;
    if rest.len() != expected {
        return Err(RowCodecError::Invalid(
            "array payload length does not match declared element count".to_string(),
        ));
    }
    let mut items: Vec<Option<T>> = Vec::new();
    items
        .try_reserve_exact(nulls.len())
        .map_err(|_| RowCodecError::Invalid("failed to reserve array elements".to_string()))?;
    let mut chunks = rest.chunks_exact(width);
    for is_null in nulls {
        if *is_null {
            items.push(None);
        } else {
            let chunk = chunks.next().ok_or_else(|| {
                RowCodecError::Invalid("array payload truncated at element".to_string())
            })?;
            items.push(Some(convert(chunk)?));
        }
    }
    Ok(items)
}

/// 配列要素列（フレームヘッダを含まない本文）を、宣言された `count` 個の要素へ
/// 構造検証しながらデコードする。要素列の実バイト長が `count` 個をちょうど消費
/// しない場合（不足・余剰いずれも）は `Err`。TEXT 要素は UTF-8、REAL／DOUBLE は
/// 有限性、DATE／TIMESTAMP は値域を検証する。
/// [`scan_scalar_columns_validated`]（構造検証のみ・結果を捨てる用途にも使う）と
/// [`ArrayRef::to_value`]（値を複製して返す用途）の両方から呼ばれる。
fn decode_array_elements(
    elem: ArrayElemType,
    bytes: &[u8],
    count: u32,
    flags: u8,
) -> Result<ArrayValue> {
    let (nulls, rest) = split_array_null_bitmap(bytes, count as usize, flags)?;
    match elem {
        ArrayElemType::Text => {
            let mut offset = 0usize;
            let mut items: Vec<Option<String>> = Vec::new();
            items.try_reserve_exact(nulls.len()).map_err(|_| {
                RowCodecError::Invalid("failed to reserve array text elements".to_string())
            })?;
            for is_null in &nulls {
                if *is_null {
                    items.push(None);
                    continue;
                }
                let len_end = offset.checked_add(4).ok_or_else(|| {
                    RowCodecError::Invalid(
                        "offset overflow before array text element length".to_string(),
                    )
                })?;
                let len_bytes = rest.get(offset..len_end).ok_or_else(|| {
                    RowCodecError::Invalid(
                        "array payload truncated at element length field".to_string(),
                    )
                })?;
                let item_len = u32::from_le_bytes(array_le_bytes::<4>(len_bytes)?);
                let item_end = len_end.checked_add(item_len as usize).ok_or_else(|| {
                    RowCodecError::Invalid("offset overflow after array text element".to_string())
                })?;
                let item_bytes = rest.get(len_end..item_end).ok_or_else(|| {
                    RowCodecError::Invalid(
                        "array payload truncated at text element field".to_string(),
                    )
                })?;
                let item = std::str::from_utf8(item_bytes)
                    .map_err(|_| {
                        RowCodecError::Invalid("array text element is not valid UTF-8".to_string())
                    })?
                    .to_string();
                offset = item_end;
                items.push(Some(item));
            }
            if offset != rest.len() {
                return Err(RowCodecError::Invalid(
                    "array payload has trailing bytes beyond declared elements".to_string(),
                ));
            }
            Ok(ArrayValue::Text(items))
        }
        ArrayElemType::Bool => {
            decode_fixed_array_elements(rest, &nulls, 1, |c| match c.first().copied() {
                Some(ARRAY_BOOL_FALSE_BYTE) => Ok(false),
                Some(ARRAY_BOOL_TRUE_BYTE) => Ok(true),
                other => Err(RowCodecError::Invalid(format!(
                    "unknown array bool element byte: {other:?}"
                ))),
            })
            .map(ArrayValue::Bool)
        }
        ArrayElemType::Integer => decode_fixed_array_elements(rest, &nulls, 4, |c| {
            Ok(i32::from_le_bytes(array_le_bytes::<4>(c)?))
        })
        .map(ArrayValue::Integer),
        ArrayElemType::BigInt => decode_fixed_array_elements(rest, &nulls, 8, |c| {
            Ok(i64::from_le_bytes(array_le_bytes::<8>(c)?))
        })
        .map(ArrayValue::BigInt),
        ArrayElemType::Real => decode_fixed_array_elements(rest, &nulls, 4, |c| {
            let v = f32::from_le_bytes(array_le_bytes::<4>(c)?);
            if !v.is_finite() {
                return Err(RowCodecError::Invalid(
                    "persisted array REAL element is not finite".to_string(),
                ));
            }
            Ok(crate::scalar_float::canonicalize_real(v))
        })
        .map(ArrayValue::Real),
        ArrayElemType::Double => decode_fixed_array_elements(rest, &nulls, 8, |c| {
            let v = f64::from_le_bytes(array_le_bytes::<8>(c)?);
            if !v.is_finite() {
                return Err(RowCodecError::Invalid(
                    "persisted array DOUBLE PRECISION element is not finite".to_string(),
                ));
            }
            Ok(crate::scalar_float::canonicalize_double(v))
        })
        .map(ArrayValue::Double),
        ArrayElemType::Date => decode_fixed_array_elements(rest, &nulls, 4, |c| {
            let v = i32::from_le_bytes(array_le_bytes::<4>(c)?);
            if !crate::datetime::validate_date_days(v) {
                return Err(RowCodecError::Invalid(
                    "persisted array DATE element out of range".to_string(),
                ));
            }
            Ok(v)
        })
        .map(ArrayValue::Date),
        ArrayElemType::Timestamp => decode_fixed_array_elements(rest, &nulls, 8, |c| {
            let v = i64::from_le_bytes(array_le_bytes::<8>(c)?);
            if !crate::datetime::validate_timestamp_micros(v) {
                return Err(RowCodecError::Invalid(
                    "persisted array TIMESTAMP element out of range".to_string(),
                ));
            }
            Ok(v)
        })
        .map(ArrayValue::Timestamp),
        ArrayElemType::Uuid => decode_fixed_array_elements(rest, &nulls, 16, |c| {
            Ok(Uuid::from_bytes(array_le_bytes::<16>(c)?))
        })
        .map(ArrayValue::Uuid),
    }
}

/// 配列列 1 個分のフレーム（presence の直後から）をバッファから読み取る。
/// `max_len_allowed` は宣言スキーマの `max_len`（[`ArrayType::max_len`]）。
/// 戻り値は検証済みの [`ArrayRef`] と、フレームを読み終えた後のオフセット。
fn parse_array_frame<'a>(
    buf: &'a [u8],
    offset: usize,
    elem: ArrayElemType,
    max_len_allowed: u32,
) -> Result<(ArrayRef<'a>, usize)> {
    let flags = *buf.get(offset).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at array flags field".to_string())
    })?;
    if flags != ARRAY_FLAGS_NONE && flags != ARRAY_FLAG_HAS_NULLS {
        return Err(RowCodecError::Invalid(format!(
            "unknown array flags byte: {flags}"
        )));
    }
    let mut offset = offset.checked_add(1).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after array flags field".to_string())
    })?;

    let count_bytes = buf
        .get(
            offset..offset.checked_add(4).ok_or_else(|| {
                RowCodecError::Invalid(
                    "offset overflow before array element count field".to_string(),
                )
            })?,
        )
        .ok_or_else(|| {
            RowCodecError::Invalid("row buffer truncated at array element count field".to_string())
        })?;
    let count_arr: [u8; 4] = count_bytes.try_into().map_err(|_| {
        RowCodecError::Invalid("array element count field is not 4 bytes".to_string())
    })?;
    let count = u32::from_le_bytes(count_arr);
    if count > max_len_allowed || count > MAX_ARRAY_ELEMENTS {
        return Err(RowCodecError::Invalid(format!(
            "array element count {count} exceeds limit {}",
            max_len_allowed.min(MAX_ARRAY_ELEMENTS)
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after array element count field".to_string())
    })?;

    let payload_len_bytes = buf
        .get(
            offset..offset.checked_add(4).ok_or_else(|| {
                RowCodecError::Invalid(
                    "offset overflow before array payload length field".to_string(),
                )
            })?,
        )
        .ok_or_else(|| {
            RowCodecError::Invalid("row buffer truncated at array payload length field".to_string())
        })?;
    let payload_len_arr: [u8; 4] = payload_len_bytes.try_into().map_err(|_| {
        RowCodecError::Invalid("array payload length field is not 4 bytes".to_string())
    })?;
    let payload_len = u32::from_le_bytes(payload_len_arr);
    if payload_len > MAX_ARRAY_PAYLOAD_LEN {
        return Err(RowCodecError::Invalid(format!(
            "array payload length {payload_len} exceeds limit {MAX_ARRAY_PAYLOAD_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after array payload length field".to_string())
    })?;

    let payload_end = offset.checked_add(payload_len as usize).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after array payload field".to_string())
    })?;
    let payload_bytes = buf.get(offset..payload_end).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at array payload field".to_string())
    })?;

    // 構造・UTF-8・要素数・NULL ビットマップの全検証をここで行う（未参照列でも
    // 弱めない。Issue #350 と同じ方針）。検証済みの `payload_bytes` を `ArrayRef`
    // へそのまま渡すため、呼び出し元・`ArrayRef::to_value` の再デコードは同じ検証を
    // 再実行するだけで追加のエラー分岐を要さない。
    decode_array_elements(elem, payload_bytes, count, flags)?;

    Ok((
        ArrayRef {
            elem,
            count,
            flags,
            bytes: payload_bytes,
        },
        payload_end,
    ))
}

/// NUMERIC 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + `unscaled`（`i128` LE 16 バイト）。TABLE-13〔検討中〕・
/// TASK-197、Issue #885・D3）。scale は行に持たないため列型に依存しない
/// 固定長。[`SCALAR_BOOL_ENTRY_LEN`] と同じ理由で
/// `tenant::validate_set_assignments` の事前累計検証と共有する。
pub(crate) const SCALAR_NUMERIC_ENTRY_LEN: u32 = 17;

/// UUID 値 1 個をスカラーペイロードへ書き込んだ場合のフレーム込みバイト数
/// （presence(1) + 16 バイト生値。TABLE-13〔検討中〕・TASK-197、Issue #887）。
/// [`SCALAR_NUMERIC_ENTRY_LEN`] と同じ理由で `tenant::validate_set_assignments`
/// の事前累計検証と共有する。
pub(crate) const SCALAR_UUID_ENTRY_LEN: u32 = 17;

/// デコード結果。行レベルの RLS フィールド（`tenant_id`・`visibility`）と、
/// スキーマの列順に対応する値列を保持する。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRow {
    pub tenant_id: String,
    pub visibility: Visibility,
    pub values: Vec<Value>,
}

/// [`TableSchema`] の列定義（列順・型・nullable）に従い、行データをバイト列へ
/// エンコードする（TABLE-7）。`values` はスキーマの列順に対応させる。
///
/// - `values.len()` がスキーマの列数を超える場合は `Err`。
/// - 末尾の値が不足する場合、対応する列が nullable なら `Value::Null` を
///   補って扱う（TABLE-5: `ALTER TABLE ADD COLUMN` で追加された列を持たない
///   既存行の書き込み経路を想定）。non-nullable な列が不足する場合は `Err`。
/// - non-nullable 列へ `Value::Null` を渡した場合は `Err`。
/// - `Value::Vector` の次元がスキーマの `VECTOR(N)` と一致しない場合は `Err`。
/// - 長さフィールドの数値変換はすべて `try_from` で行い、失敗（上限超過）を
///   `Err` とする（`as` キャストによる剰余切り詰めは行わない。TABLE-7 の核心）。
pub fn encode_row(
    schema: &TableSchema,
    tenant_id: &str,
    visibility: Visibility,
    values: &[Value],
) -> Result<Vec<u8>> {
    // この「フル行」v1 物理フォーマットは production 経路では使われない
    // （production は `storage.rs` のヘッダ + `encode_scalar_columns` のスカラー
    // ペイロード形式のみを書く）。削除済み列（墓標）を持つスキーマの物理配置は
    // この形式に反映していないため、混同を避け fail-closed に拒否する
    // （TABLE-19・Issue #901）。
    if !schema.dropped_slots().is_empty() {
        return Err(RowCodecError::Invalid(
            "encode_row does not support schemas with dropped columns".to_string(),
        ));
    }
    if values.len() > schema.columns.len() {
        return Err(RowCodecError::Invalid(format!(
            "too many values: schema has {} columns, got {}",
            schema.columns.len(),
            values.len()
        )));
    }

    if tenant_id.is_empty() {
        return Err(RowCodecError::Invalid(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = tenant_id.as_bytes();
    // MAX_TENANT_ID_LEN(= u8::MAX) はヘッダの長さフィールド幅そのものであり、
    // u8::try_from の失敗が上限超過検出を兼ねる（`tenant_len > MAX_TENANT_ID_LEN` は
    // u8 の値域上常に false になるため、冗長な比較を書かない）。
    let tenant_len = u8::try_from(tenant_bytes.len()).map_err(|_| {
        RowCodecError::Invalid(format!(
            "tenant_id length {} exceeds limit {MAX_TENANT_ID_LEN}",
            tenant_bytes.len()
        ))
    })?;

    let mut buf = Vec::new();
    buf.push(ROW_CODEC_FORMAT_VERSION);
    buf.push(visibility.to_byte());
    buf.push(tenant_len);
    buf.extend_from_slice(tenant_bytes);

    for (idx, column) in schema.columns.iter().enumerate() {
        // スキーマの列数が values より多い場合、末尾の欠落列は「値なし」として
        // 扱う（TABLE-5 前提: nullable なら Null、non-nullable なら下の分岐で Err）。
        let value = values.get(idx).unwrap_or(&Value::Null);
        match value {
            Value::Null => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is missing",
                        column.name
                    )));
                }
                buf.push(PRESENCE_NULL);
            }
            Value::Text(text) => {
                if !matches!(column.ty, ColumnType::Text) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Text value, got Text",
                        column.name
                    )));
                }
                let text_bytes = text.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "text field too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Real(v) => {
                if !matches!(column.ty, ColumnType::Real) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Real value, got Real",
                        column.name
                    )));
                }
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: REAL value must be finite",
                        column.name
                    )));
                }
                // F4: 公開 API（typed insert 等）経由で SQL リテラルの
                // 解析（scalar_float::parse_real）を経ずに `-0.0` が渡り得るため、
                // 永続化バイト列を確定させる直前に正規化する。
                let v = crate::scalar_float::canonicalize_real(*v);
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Double(v) => {
                if !matches!(column.ty, ColumnType::Double) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Double value, got Double",
                        column.name
                    )));
                }
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: DOUBLE PRECISION value must be finite",
                        column.name
                    )));
                }
                // F4: canonicalize_real と同じ理由（-0.0 正規化）。
                let v = crate::scalar_float::canonicalize_double(*v);
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            // ENUM 列の値（Issue #890 D3）。行バイト表現は TEXT と同一フレーム
            // （presence + `u32` LE 長 + UTF-8 本体）を共有するが、書き込み前に
            // 語彙を検査する多層防御を持つ（束縛層〔`sql::parser::bind_enum_literal`〕
            // に加え、Rust API から直接渡された `Value::Enum` もここで拒否する）。
            Value::Enum(label) => {
                let def = match &column.ty {
                    ColumnType::Enum(def) => def,
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Vector(_)
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
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Enum value, got Enum",
                            column.name
                        )))
                    }
                };
                if def.validate_label(label).is_err() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} does not accept label {label:?} for enum type {:?}",
                        column.name,
                        def.name()
                    )));
                }
                let text_bytes = label.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "enum label too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Vector(vector) => {
                let expected_dim = match &column.ty {
                    ColumnType::Vector(dim) => *dim,
                    ColumnType::Text
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
                    | ColumnType::Enum(_)
                    | ColumnType::Numeric { .. }
                    | ColumnType::Uuid => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Vector value, got Vector",
                            column.name
                        )))
                    }
                };
                let dim = u32::try_from(vector.len()).map_err(|_| {
                    RowCodecError::Invalid(format!("embedding dim too large: {}", vector.len()))
                })?;
                if dim != expected_dim {
                    return Err(RowCodecError::Invalid(format!(
                        "embedding dim mismatch for column {:?}: expected {expected_dim}, got {dim}",
                        column.name
                    )));
                }
                if dim > MAX_EMBEDDING_DIM {
                    return Err(RowCodecError::Invalid(format!(
                        "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&dim.to_le_bytes());
                for v in vector {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            Value::Integer(v) => {
                if !matches!(column.ty, ColumnType::Integer) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Integer value, got Integer",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::BigInt(v) => {
                if !matches!(column.ty, ColumnType::BigInt) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-BigInt value, got BigInt",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Bool(b) => {
                if !matches!(column.ty, ColumnType::Boolean) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Boolean value, got Boolean",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.push(if *b { BOOL_TRUE_BYTE } else { BOOL_FALSE_BYTE });
            }
            Value::Date(days) => {
                if !matches!(column.ty, ColumnType::Date) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Date value, got Date",
                        column.name
                    )));
                }
                if !crate::datetime::validate_date_days(*days) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: date value out of range: {days}",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&days.to_le_bytes());
            }
            Value::Timestamp(micros) => {
                if !matches!(column.ty, ColumnType::Timestamp) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Timestamp value, got Timestamp",
                        column.name
                    )));
                }
                if !crate::datetime::validate_timestamp_micros(*micros) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: timestamp value out of range: {micros}",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&micros.to_le_bytes());
            }
            Value::Array(array_value) => {
                let array_ty = match &column.ty {
                    ColumnType::Array(array_ty) => *array_ty,
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Vector(_)
                    | ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Boolean
                    | ColumnType::Date
                    | ColumnType::Timestamp
                    | ColumnType::Bytea
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Enum(_)
                    | ColumnType::Numeric { .. }
                    | ColumnType::Uuid => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Array value, got Array",
                            column.name
                        )))
                    }
                };
                buf.push(PRESENCE_VALUE);
                write_array_value(&mut buf, array_ty, array_value)?;
            }
            Value::Bytes(bytes) => {
                if !matches!(column.ty, ColumnType::Bytea) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Bytea value, got Bytea",
                        column.name
                    )));
                }
                let byte_len = u32::try_from(bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!("bytea field too long: {} bytes", bytes.len()))
                })?;
                if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "bytea field length {byte_len} exceeds limit {}",
                        crate::bytea::MAX_BYTEA_FIELD_LEN
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&byte_len.to_le_bytes());
                buf.extend_from_slice(bytes);
            }
            Value::Json(text) => {
                encode_json_value(&mut buf, column, text)?;
            }
            Value::Numeric(d) => {
                let scale = match column.ty {
                    ColumnType::Numeric { precision, scale } => {
                        if !d.fits_precision(precision) {
                            return Err(RowCodecError::Invalid(format!(
                                "column {:?} numeric value out of range for precision {precision}",
                                column.name
                            )));
                        }
                        scale
                    }
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Vector(_)
                    | ColumnType::Boolean
                    | ColumnType::Date
                    | ColumnType::Timestamp
                    | ColumnType::Array(_)
                    | ColumnType::Bytea
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Enum(_)
                    | ColumnType::Uuid => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Numeric value, got Numeric",
                            column.name
                        )))
                    }
                };
                if d.scale() != scale {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects numeric scale {scale}, got {}",
                        column.name,
                        d.scale()
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&d.unscaled().to_le_bytes());
            }
            Value::Uuid(u) => {
                if !matches!(column.ty, ColumnType::Uuid) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Uuid value, got Uuid",
                        column.name
                    )));
                }
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(u.as_bytes());
            }
        }
    }

    Ok(buf)
}

/// `JSON`／`JSONB` 列 1 個分を `buf` へ書き込む（[`encode_row`]・
/// [`encode_scalar_columns`]・[`merge_encode_scalar_columns`] が共有する唯一の
/// エンコード実装）。Issue #889 D2「格納時検証の単一チョークポイント」に従い、
/// `JSON` 列は [`crate::json::validate_json_column_text`] で構文検証のみ行い
/// 入力テキストをそのまま格納し、`JSONB` 列は [`crate::json::canonicalize_jsonb_text`]
/// で得た正規化形と入力テキストが一致することを検証する（束縛層は常に正規化済みの
/// テキストを渡す契約であり、不一致は API 誤用として拒否する）。これにより
/// Rust API（`tenant::insert_typed_row` 等）経由でも未検証・未正規化の JSON が
/// 格納されない（TEXT と同じ presence + `u32` LE 長 + UTF-8 本体の枠を共有する）。
fn encode_json_value(
    buf: &mut Vec<u8>,
    column: &crate::catalog::ColumnDef,
    text: &str,
) -> Result<()> {
    validate_json_column_value(column, text)?;
    let text_bytes = text.as_bytes();
    let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
        RowCodecError::Invalid(format!("json field too long: {} bytes", text_bytes.len()))
    })?;
    if text_len > MAX_TEXT_FIELD_LEN {
        return Err(RowCodecError::Invalid(format!(
            "json field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
        )));
    }
    buf.push(PRESENCE_VALUE);
    buf.extend_from_slice(&text_len.to_le_bytes());
    buf.extend_from_slice(text_bytes);
    Ok(())
}

/// [`encode_row`] の逆変換。欠落・不正値・切り詰め・未知タグをすべて `Err` で拒否する
/// （fail-closed。黙殺フォールバックで既定値へ落とさない）。添字アクセス `[]` ではなく
/// `get()`・`checked_add` を使い、境界外アクセス・オーバーフローを未定義動作にしない。
///
/// バッファが列の途中で終わっている場合、その列以降は「nullable なら `Value::Null`、
/// non-nullable なら `Err`」として扱う（TABLE-5: `ALTER TABLE ADD COLUMN` 後の
/// 既存行デコード前提）。
pub fn decode_row(schema: &TableSchema, buf: &[u8]) -> Result<DecodedRow> {
    // `encode_row` と同じ理由で、削除済み列（墓標）を持つスキーマは fail-closed
    // に拒否する（TABLE-19・Issue #901）。
    if !schema.dropped_slots().is_empty() {
        return Err(RowCodecError::Invalid(
            "decode_row does not support schemas with dropped columns".to_string(),
        ));
    }
    let version = *buf
        .first()
        .ok_or_else(|| RowCodecError::Invalid("row buffer is empty".to_string()))?;
    if version != ROW_CODEC_FORMAT_VERSION {
        return Err(RowCodecError::Invalid(format!(
            "unsupported row format version: {version}"
        )));
    }

    let visibility_byte = *buf.get(1).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at visibility field".to_string())
    })?;
    let visibility = Visibility::from_byte(visibility_byte)
        .map_err(|e| RowCodecError::Invalid(format!("invalid visibility byte: {e}")))?;

    // tenant_len は u8 のヘッダフィールドとして読み出すため、値域は常に
    // 0..=MAX_TENANT_ID_LEN(= u8::MAX) に収まる（上限超過チェックは不要）。
    let tenant_len = *buf.get(2).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at tenant_len field".to_string())
    })?;

    let mut offset = 3usize;
    let tenant_end = offset.checked_add(tenant_len as usize).ok_or_else(|| {
        RowCodecError::Invalid("offset overflow after tenant_len field".to_string())
    })?;
    let tenant_bytes = buf.get(offset..tenant_end).ok_or_else(|| {
        RowCodecError::Invalid("row buffer truncated at tenant_id field".to_string())
    })?;
    if tenant_bytes.is_empty() {
        return Err(RowCodecError::Invalid(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_id = std::str::from_utf8(tenant_bytes)
        .map_err(|_| RowCodecError::Invalid("tenant_id is not valid UTF-8".to_string()))?
        .to_string();
    offset = tenant_end;

    let mut values = Vec::with_capacity(schema.columns.len());
    for column in &schema.columns {
        // バッファ末尾に達した場合、以降の列はすべて「欠落」として扱う
        // （TABLE-5: ADD COLUMN 後の既存行を NULL として読む）。
        let presence = match buf.get(offset) {
            Some(&b) => b,
            None => {
                // 欠落列は `DEFAULT` があれば既定値で補う（`scan_scalar_columns_validated`
                // と同じ規則。Issue #1169）。
                if column.default.is_some() {
                    let scalar = column_default_scalar(column).map_err(|_| {
                        RowCodecError::Invalid(format!(
                            "column {:?} has an invalid DEFAULT for its type",
                            column.name
                        ))
                    })?;
                    let value = match scalar {
                        Some(ScalarRef::Text(t)) => Value::Text(t.to_string()),
                        Some(ScalarRef::Integer(v)) => Value::Integer(v),
                        Some(ScalarRef::BigInt(v)) => Value::BigInt(v),
                        Some(ScalarRef::Real(v)) => Value::Real(v),
                        Some(ScalarRef::Double(v)) => Value::Double(v),
                        Some(ScalarRef::Numeric(d)) => Value::Numeric(d),
                        Some(ScalarRef::Bool(b)) => Value::Bool(b),
                        Some(ScalarRef::Date(d)) => Value::Date(d),
                        Some(ScalarRef::Enum(t)) => Value::Enum(t.to_string()),
                        Some(ScalarRef::Uuid(u)) => Value::Uuid(u),
                        Some(ScalarRef::Timestamp(t)) => Value::Timestamp(t),
                        _ => {
                            return Err(RowCodecError::Invalid(format!(
                                "column {:?} has an invalid DEFAULT for its type",
                                column.name
                            )))
                        }
                    };
                    values.push(value);
                    continue;
                }
                if column.nullable {
                    values.push(Value::Null);
                    continue;
                } else {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but row buffer is truncated",
                        column.name
                    )));
                }
            }
        };
        offset = offset.checked_add(1).ok_or_else(|| {
            RowCodecError::Invalid("offset overflow after presence field".to_string())
        })?;

        match presence {
            PRESENCE_NULL => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is NULL",
                        column.name
                    )));
                }
                values.push(Value::Null);
            }
            PRESENCE_VALUE => match &column.ty {
                ColumnType::Text | ColumnType::Enum(_) => {
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before text length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at text length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("text length field is not 4 bytes".to_string())
                    })?;
                    let text_len = u32::from_le_bytes(len_arr);
                    if text_len > MAX_TEXT_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after text length field".to_string(),
                        )
                    })?;
                    let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after text field".to_string())
                    })?;
                    let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at text field".to_string())
                    })?;
                    let text = std::str::from_utf8(text_bytes)
                        .map_err(|_| {
                            RowCodecError::Invalid("text field is not valid UTF-8".to_string())
                        })?
                        .to_string();
                    offset = text_end;
                    // ENUM 列は decode 時に現行語彙（`ALTER TYPE ... ADD VALUE`
                    // で単調増加する `EnumTypeDef::labels`）との照合を行う
                    // （codex-review P1 指摘・Issue #890: 破損行〔手書き・
                    // バグ由来〕が持つ語彙外ラベルを `Value::Enum` として
                    // 通すと、投影・等価フィルタ・二次索引へ任意文字列が
                    // 流出しうるため）。ラベルは削除されない契約のため、
                    // 過去に正当だった値は将来にわたって有効であり続ける
                    // （this 検査は「現行スキーマの語彙に含まれるか」であり
                    // 「書込み時点で有効だったか」を後退させるものではない）。
                    if let ColumnType::Enum(def) = &column.ty {
                        if !def.contains(&text) {
                            return Err(RowCodecError::Invalid(format!(
                                "enum value {text:?} is not a valid label of type {:?}",
                                def.name()
                            )));
                        }
                        values.push(Value::Enum(text));
                    } else {
                        values.push(Value::Text(text));
                    }
                }
                ColumnType::Real => {
                    let bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before real field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid("row buffer truncated at real field".to_string())
                        })?;
                    let arr: [u8; 4] = bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("real field is not 4 bytes".to_string())
                    })?;
                    let v = f32::from_le_bytes(arr);
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: persisted REAL value is not finite",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after real field".to_string())
                    })?;
                    values.push(Value::Real(crate::scalar_float::canonicalize_real(v)));
                }
                ColumnType::Double => {
                    let bytes = buf
                        .get(
                            offset..offset.checked_add(8).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before double field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at double field".to_string(),
                            )
                        })?;
                    let arr: [u8; 8] = bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("double field is not 8 bytes".to_string())
                    })?;
                    let v = f64::from_le_bytes(arr);
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: persisted DOUBLE PRECISION value is not finite",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after double field".to_string())
                    })?;
                    values.push(Value::Double(crate::scalar_float::canonicalize_double(v)));
                }
                ColumnType::Vector(expected_dim) => {
                    let expected_dim = *expected_dim;
                    let dim_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before vector dim field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at vector dim field".to_string(),
                            )
                        })?;
                    let dim_arr: [u8; 4] = dim_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("vector dim field is not 4 bytes".to_string())
                    })?;
                    let dim = u32::from_le_bytes(dim_arr);
                    if dim > MAX_EMBEDDING_DIM {
                        return Err(RowCodecError::Invalid(format!(
                            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
                        )));
                    }
                    if dim != expected_dim {
                        return Err(RowCodecError::Invalid(format!(
                            "embedding dim mismatch for column {:?}: expected {expected_dim}, got {dim}",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after vector dim field".to_string())
                    })?;
                    let vector_bytes_len = (dim as usize).checked_mul(4).ok_or_else(|| {
                        RowCodecError::Invalid("vector byte length overflow".to_string())
                    })?;
                    let vector_end = offset.checked_add(vector_bytes_len).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after vector field".to_string())
                    })?;
                    let vector_bytes = buf.get(offset..vector_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at vector field".to_string())
                    })?;
                    // 上限検証済みの dim に基づくため、無制限確保にはならない。
                    let mut vector = Vec::with_capacity(dim as usize);
                    for chunk in vector_bytes.as_chunks::<4>().0 {
                        vector.push(f32::from_le_bytes(*chunk));
                    }
                    offset = vector_end;
                    values.push(Value::Vector(vector));
                }
                ColumnType::Integer => {
                    let field_end = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after integer field".to_string())
                    })?;
                    let field_bytes = buf.get(offset..field_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at integer field".to_string())
                    })?;
                    let arr: [u8; 4] = field_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("integer field is not 4 bytes".to_string())
                    })?;
                    offset = field_end;
                    values.push(Value::Integer(i32::from_le_bytes(arr)));
                }
                ColumnType::BigInt => {
                    let field_end = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after bigint field".to_string())
                    })?;
                    let field_bytes = buf.get(offset..field_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at bigint field".to_string())
                    })?;
                    let arr: [u8; 8] = field_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("bigint field is not 8 bytes".to_string())
                    })?;
                    offset = field_end;
                    values.push(Value::BigInt(i64::from_le_bytes(arr)));
                }
                ColumnType::Boolean => {
                    let byte = *buf.get(offset).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "row buffer truncated at boolean value field".to_string(),
                        )
                    })?;
                    let b = match byte {
                        BOOL_FALSE_BYTE => false,
                        BOOL_TRUE_BYTE => true,
                        other => {
                            return Err(RowCodecError::Invalid(format!(
                                "unknown boolean value byte: {other}"
                            )))
                        }
                    };
                    offset = offset.checked_add(1).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after boolean value field".to_string(),
                        )
                    })?;
                    values.push(Value::Bool(b));
                }
                ColumnType::Date => {
                    let days_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before date value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at date value field".to_string(),
                            )
                        })?;
                    let days_arr: [u8; 4] = days_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("date value field is not 4 bytes".to_string())
                    })?;
                    let days = i32::from_le_bytes(days_arr);
                    if !crate::datetime::validate_date_days(days) {
                        return Err(RowCodecError::Invalid(format!(
                            "date value {days} is out of the representable range"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after date value field".to_string())
                    })?;
                    values.push(Value::Date(days));
                }
                ColumnType::Timestamp => {
                    let micros_bytes = buf
                        .get(
                            offset..offset.checked_add(8).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before timestamp value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at timestamp value field".to_string(),
                            )
                        })?;
                    let micros_arr: [u8; 8] = micros_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("timestamp value field is not 8 bytes".to_string())
                    })?;
                    let micros = i64::from_le_bytes(micros_arr);
                    if !crate::datetime::validate_timestamp_micros(micros) {
                        return Err(RowCodecError::Invalid(format!(
                            "timestamp value {micros} is out of the representable range"
                        )));
                    }
                    offset = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after timestamp value field".to_string(),
                        )
                    })?;
                    values.push(Value::Timestamp(micros));
                }
                ColumnType::Array(array_ty) => {
                    let (array_ref, new_offset) =
                        parse_array_frame(buf, offset, array_ty.elem(), array_ty.max_len())?;
                    offset = new_offset;
                    values.push(Value::Array(array_ref.to_value()?));
                }
                ColumnType::Bytea => {
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before bytea length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at bytea length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("bytea length field is not 4 bytes".to_string())
                    })?;
                    let byte_len = u32::from_le_bytes(len_arr);
                    if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "bytea field length {byte_len} exceeds limit {}",
                            crate::bytea::MAX_BYTEA_FIELD_LEN
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after bytea length field".to_string(),
                        )
                    })?;
                    let bytea_end = offset.checked_add(byte_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after bytea field".to_string())
                    })?;
                    let bytea_bytes = buf.get(offset..bytea_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at bytea field".to_string())
                    })?;
                    offset = bytea_end;
                    values.push(Value::Bytes(bytea_bytes.to_vec()));
                }
                ColumnType::Json | ColumnType::Jsonb => {
                    // presence + `u32` LE 長 + UTF-8 本体は `Text` と同一の枠を
                    // 共有する（Issue #889 D2）。書き込み経路（`encode_row` 系）が
                    // 検証・正規化済みの契約であるため、decode 側は TEXT と同じく
                    // 長さ上限・UTF-8 妥当性のみを検証し再パースしない。
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before json length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at json length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("json length field is not 4 bytes".to_string())
                    })?;
                    let text_len = u32::from_le_bytes(len_arr);
                    if text_len > MAX_TEXT_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "json field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after json length field".to_string(),
                        )
                    })?;
                    let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after json field".to_string())
                    })?;
                    let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                        RowCodecError::Invalid("row buffer truncated at json field".to_string())
                    })?;
                    let text = std::str::from_utf8(text_bytes)
                        .map_err(|_| {
                            RowCodecError::Invalid("json field is not valid UTF-8".to_string())
                        })?
                        .to_string();
                    offset = text_end;
                    values.push(Value::Json(text));
                }
                ColumnType::Numeric { precision, scale } => {
                    let unscaled_bytes = buf
                        .get(
                            offset..offset.checked_add(16).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before numeric value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at numeric value field".to_string(),
                            )
                        })?;
                    let unscaled_arr: [u8; 16] = unscaled_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("numeric value field is not 16 bytes".to_string())
                    })?;
                    let unscaled = i128::from_le_bytes(unscaled_arr);
                    let decimal = Decimal::from_parts(unscaled, *scale).map_err(|_| {
                        RowCodecError::Invalid(format!(
                            "column {:?} numeric scale out of range",
                            column.name
                        ))
                    })?;
                    if !decimal.fits_precision(*precision) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} numeric value out of range for precision {precision}",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(16).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after numeric value field".to_string(),
                        )
                    })?;
                    values.push(Value::Numeric(decimal));
                }
                ColumnType::Uuid => {
                    let uuid_bytes: [u8; 16] = buf
                        .get(
                            offset..offset.checked_add(16).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before uuid value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "row buffer truncated at uuid value field".to_string(),
                            )
                        })?
                        .try_into()
                        .map_err(|_| {
                            RowCodecError::Invalid("uuid value field is not 16 bytes".to_string())
                        })?;
                    offset = offset.checked_add(16).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after uuid value field".to_string())
                    })?;
                    values.push(Value::Uuid(Uuid::from_bytes(uuid_bytes)));
                }
            },
            other => {
                return Err(RowCodecError::Invalid(format!(
                    "unknown presence byte: {other}"
                )))
            }
        }
    }

    if offset != buf.len() {
        return Err(RowCodecError::Invalid(
            "row buffer has trailing bytes beyond declared columns".to_string(),
        ));
    }

    Ok(DecodedRow {
        tenant_id,
        visibility,
        values,
    })
}

/// `sql::exec`（TASK-75、対象ビヘイビア: SQL-2）から呼ばれる、スキーマの非
/// `VECTOR` 列（`Text` 列）のみを列順にエンコードするペイロード。`storage.rs::RowInput`
/// は `embedding`（`VECTOR` 列 1 本）と不透明な `metadata` バイト列しか持たないため、
/// `VECTOR` 列は `embedding` スロットへ、それ以外は本関数の出力を `metadata` へ格納する
/// という規約を SQL 表層のローカルな契約として定義する（`encode_row`/`decode_row` の
/// フルスキーマコーデックは `tenant_id`/`visibility` ヘッダごと持つため、`storage.rs` 側で
/// 既に保持しているそれらと二重管理・二重保存になってしまい、本関数では使わない）。
///
/// `values` は [`TableSchema::columns`] の列順に対応させる（`VECTOR` 列の位置は
/// 無条件にスキップされ、`values` に何を渡しても読まれない）。欠落・上限超過・
/// 型不一致の規則は [`encode_row`] と同一（nullable 列の末尾欠落は `Value::Null`、
/// non-nullable な欠落は `Err`）。
pub fn encode_scalar_columns(schema: &TableSchema, values: &[Value]) -> Result<Vec<u8>> {
    if values.len() > schema.columns.len() {
        return Err(RowCodecError::Invalid(format!(
            "too many values: schema has {} columns, got {}",
            schema.columns.len(),
            values.len()
        )));
    }

    // 累計出力バイト数を確保前に検証する（[`MAX_SCALAR_PAYLOAD_LEN`] 参照）。
    // 列ごとの `MAX_TEXT_FIELD_LEN` 検査だけでは、多数の `TEXT` 列を持つ
    // スキーマで `buf` が列数倍に膨らみ得るため、1 バイト書き込むより前に
    // 累計上限を跨いでいないか判定してから `push`/`extend_from_slice` する
    // （`buf` 自体は `MAX_SCALAR_PAYLOAD_LEN` を超えて確保されない）。
    let mut buf = Vec::new();
    let mut total_len: u32 = 0;
    let mut reserve = |buf: &mut Vec<u8>, additional: u32| -> Result<()> {
        total_len = total_len
            .checked_add(additional)
            .ok_or_else(|| RowCodecError::Invalid("scalar payload length overflow".to_string()))?;
        if total_len > MAX_SCALAR_PAYLOAD_LEN {
            return Err(RowCodecError::Invalid(format!(
                "scalar payload length {total_len} exceeds limit {MAX_SCALAR_PAYLOAD_LEN}"
            )));
        }
        // `try_reserve_exact` の `additional` は `buf.len()` からの追加要素数であり
        // `buf.capacity()` からの差分ではない（アロケータが要求量より多い容量を
        // 返した場合、`capacity()` 基準の差分計算は必要量を過小に見積もり、
        // 後続の `push`/`extend_from_slice` が意図した `Invalid` エラーではなく
        // 暗黙の再確保（OOM 時は panic）経路へ落ちてしまう。Cursor Bugbot Low
        // 指摘・PR #989）。`buf.len()` 基準で不足分のみを計算する。
        if buf.capacity() < total_len as usize {
            let needed = (total_len as usize).saturating_sub(buf.len());
            buf.try_reserve_exact(needed).map_err(|_| {
                RowCodecError::Invalid("failed to reserve scalar payload buffer".to_string())
            })?;
        }
        Ok(())
    };
    for slot in schema.physical_slots() {
        let (idx, column) = match slot {
            crate::catalog::PhysicalSlot::Live(idx, column) => (idx, column),
            crate::catalog::PhysicalSlot::Dropped(_) => {
                // 削除済み列（墓標）の物理位置には、新規に書き込む行では常に
                // NULL を書く（削除前の値を引き継がない。TABLE-19 D1・
                // Issue #901）。
                reserve(&mut buf, 1)?;
                buf.push(PRESENCE_NULL);
                continue;
            }
        };
        if matches!(column.ty, ColumnType::Vector(_)) {
            // VECTOR 列は storage.rs 側の embedding スロットが担当するため、
            // スカラーペイロードには一切含めない（値の有無・内容を問わずスキップ）。
            continue;
        }
        let value = values.get(idx).unwrap_or(&Value::Null);
        match value {
            Value::Null => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is missing",
                        column.name
                    )));
                }
                reserve(&mut buf, 1)?;
                buf.push(PRESENCE_NULL);
            }
            Value::Text(text) => {
                if !matches!(column.ty, ColumnType::Text) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Text value, got Text",
                        column.name
                    )));
                }
                let text_bytes = text.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "text field too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                // presence(1) + 長さ(4) + 本文（text_len）の合計を確保前に検証する。
                let entry_len = scalar_text_entry_len(text_len)?;
                reserve(&mut buf, entry_len)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Real(v) => {
                if !matches!(column.ty, ColumnType::Real) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Real value, got Real",
                        column.name
                    )));
                }
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: REAL value must be finite",
                        column.name
                    )));
                }
                // F4: -0.0 正規化（他エンコード経路と同じ理由。row_codec.rs 冒頭参照）。
                let v = crate::scalar_float::canonicalize_real(*v);
                reserve(&mut buf, SCALAR_REAL_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Double(v) => {
                if !matches!(column.ty, ColumnType::Double) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Double value, got Double",
                        column.name
                    )));
                }
                if !v.is_finite() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: DOUBLE PRECISION value must be finite",
                        column.name
                    )));
                }
                // F4: -0.0 正規化。
                let v = crate::scalar_float::canonicalize_double(*v);
                reserve(&mut buf, SCALAR_DOUBLE_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Enum(label) => {
                let def = match &column.ty {
                    ColumnType::Enum(def) => def,
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Vector(_)
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
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Enum value, got Enum",
                            column.name
                        )))
                    }
                };
                if def.validate_label(label).is_err() {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} does not accept label {label:?} for enum type {:?}",
                        column.name,
                        def.name()
                    )));
                }
                let text_bytes = label.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "enum label too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                let entry_len = scalar_text_entry_len(text_len)?;
                reserve(&mut buf, entry_len)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Vector(_) => {
                return Err(RowCodecError::Invalid(format!(
                    "column {:?} expects a non-Vector value, got Vector",
                    column.name
                )))
            }
            Value::Integer(v) => {
                if !matches!(column.ty, ColumnType::Integer) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Integer value, got Integer",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_INT4_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::BigInt(v) => {
                if !matches!(column.ty, ColumnType::BigInt) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-BigInt value, got BigInt",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_INT8_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Bool(b) => {
                if !matches!(column.ty, ColumnType::Boolean) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Boolean value, got Boolean",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_BOOL_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.push(if *b { BOOL_TRUE_BYTE } else { BOOL_FALSE_BYTE });
            }
            Value::Date(days) => {
                if !matches!(column.ty, ColumnType::Date) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Date value, got Date",
                        column.name
                    )));
                }
                if !crate::datetime::validate_date_days(*days) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: date value out of range: {days}",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_DATE_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&days.to_le_bytes());
            }
            Value::Timestamp(micros) => {
                if !matches!(column.ty, ColumnType::Timestamp) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Timestamp value, got Timestamp",
                        column.name
                    )));
                }
                if !crate::datetime::validate_timestamp_micros(*micros) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?}: timestamp value out of range: {micros}",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_TIMESTAMP_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&micros.to_le_bytes());
            }
            Value::Array(array_value) => {
                let array_ty = match &column.ty {
                    ColumnType::Array(array_ty) => *array_ty,
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Vector(_)
                    | ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Boolean
                    | ColumnType::Date
                    | ColumnType::Timestamp
                    | ColumnType::Bytea
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Enum(_)
                    | ColumnType::Numeric { .. }
                    | ColumnType::Uuid => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Array value, got Array",
                            column.name
                        )))
                    }
                };
                let entry_len = scalar_array_entry_len(array_ty.elem(), array_value)?;
                reserve(&mut buf, entry_len)?;
                buf.push(PRESENCE_VALUE);
                write_array_value(&mut buf, array_ty, array_value)?;
            }
            Value::Bytes(bytes) => {
                if !matches!(column.ty, ColumnType::Bytea) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Bytea value, got Bytea",
                        column.name
                    )));
                }
                let byte_len = u32::try_from(bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!("bytea field too long: {} bytes", bytes.len()))
                })?;
                if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "bytea field length {byte_len} exceeds limit {}",
                        crate::bytea::MAX_BYTEA_FIELD_LEN
                    )));
                }
                // フレーミング（presence(1) + 長さ(4)）は TEXT と同一のため
                // `scalar_text_entry_len` を共有する（B3）。
                let entry_len = scalar_text_entry_len(byte_len)?;
                reserve(&mut buf, entry_len)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&byte_len.to_le_bytes());
                buf.extend_from_slice(bytes);
            }
            Value::Json(text) => {
                validate_json_column_value(column, text)?;
                let text_bytes = text.as_bytes();
                let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
                    RowCodecError::Invalid(format!(
                        "json field too long: {} bytes",
                        text_bytes.len()
                    ))
                })?;
                if text_len > MAX_TEXT_FIELD_LEN {
                    return Err(RowCodecError::Invalid(format!(
                        "json field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                    )));
                }
                // フレーミング（presence(1) + 長さ(4)）は TEXT と同一のため
                // `scalar_text_entry_len` を共有する。
                let entry_len = scalar_text_entry_len(text_len)?;
                reserve(&mut buf, entry_len)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&text_len.to_le_bytes());
                buf.extend_from_slice(text_bytes);
            }
            Value::Numeric(d) => {
                let (precision, scale) = match column.ty {
                    ColumnType::Numeric { precision, scale } => (precision, scale),
                    ColumnType::Text
                    | ColumnType::Integer
                    | ColumnType::BigInt
                    | ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Vector(_)
                    | ColumnType::Boolean
                    | ColumnType::Date
                    | ColumnType::Timestamp
                    | ColumnType::Array(_)
                    | ColumnType::Bytea
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Enum(_)
                    | ColumnType::Uuid => {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Numeric value, got Numeric",
                            column.name
                        )))
                    }
                };
                if d.scale() != scale || !d.fits_precision(precision) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} numeric value does not match column NUMERIC({precision},{scale})",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_NUMERIC_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(&d.unscaled().to_le_bytes());
            }
            Value::Uuid(u) => {
                if !matches!(column.ty, ColumnType::Uuid) {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Uuid value, got Uuid",
                        column.name
                    )));
                }
                reserve(&mut buf, SCALAR_UUID_ENTRY_LEN)?;
                buf.push(PRESENCE_VALUE);
                buf.extend_from_slice(u.as_bytes());
            }
        }
    }

    Ok(buf)
}

/// `JSON`／`JSONB` 列の値検証（列型に応じた [`crate::json::validate_json_column_text`]／
/// [`crate::json::canonicalize_jsonb_text`] の呼び分け）を [`encode_json_value`]・
/// [`encode_scalar_columns`]・[`merge_encode_scalar_columns`] の 3 箇所で共有する。
fn validate_json_column_value(column: &crate::catalog::ColumnDef, text: &str) -> Result<()> {
    match &column.ty {
        ColumnType::Json => {
            crate::json::validate_json_column_text(text).map_err(|_| {
                RowCodecError::Invalid(format!("column {:?} expects valid JSON text", column.name))
            })?;
        }
        ColumnType::Jsonb => {
            let canonical = crate::json::canonicalize_jsonb_text(text).map_err(|_| {
                RowCodecError::Invalid(format!("column {:?} expects valid JSON text", column.name))
            })?;
            if canonical != text {
                return Err(RowCodecError::Invalid(format!(
                    "column {:?} expects pre-canonicalized JSONB text",
                    column.name
                )));
            }
        }
        ColumnType::Text
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Vector(_)
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Array(_)
        | ColumnType::Bytea
        | ColumnType::Enum(_)
        | ColumnType::Numeric { .. }
        | ColumnType::Uuid => {
            return Err(RowCodecError::Invalid(format!(
                "column {:?} expects a non-JSON value, got JSON",
                column.name
            )));
        }
    }
    Ok(())
}

/// UPDATE の列指定マージ（`crate::tenant::update_row_columns_unchecked`）専用の
/// マージ＋再エンコード。`existing`（[`scan_scalar_columns`] が返す借用 `&str`。
/// `VECTOR` 列・実際の `NULL` 列はいずれも `None`）を土台に、`overrides`
/// （SET 句で上書きする列 index と値）で指定された列だけを差し替えてエンコード
/// する。[`decode_scalar_columns`] のように SET 対象でない列を `Value::Text`
/// （`String` への複製）へいったん変換してから [`encode_scalar_columns`] へ渡す
/// 経路は、部分 UPDATE 1 回あたり「対象行の全 `TEXT` 列を複製する `decode` バッファ」
/// と「同程度の出力を確保する `encode` バッファ」の 2 つのピーク確保を必要とする
/// （codex-review P1 指摘・PR #989）。本関数は SET 対象でない列を `existing` の
/// 借用 `&str` のまま直接 `buf` へ書き込むことで、複製されるのは SET 句の値
/// （`overrides`。呼び出し元がクライアント入力からすでに所有している）のみに
/// 抑え、`decode` 側のピーク確保を丸ごと避ける。累計上限検証・確保前 reserve の
/// 規則は [`encode_scalar_columns`] と完全に同一（両者は将来の乖離を防ぐため
/// [`scalar_text_entry_len`]／[`MAX_SCALAR_PAYLOAD_LEN`] を共有する）。
///
/// `existing.len()` は呼び出し元（`scan_scalar_columns`）の契約により常に
/// `schema.columns.len()` と一致する想定だが、untrusted な格納済みデータに
/// 由来する不変条件のため、念のため上限超過は同じ `Err` で fail-closed に拒否する。
pub(crate) fn merge_encode_scalar_columns(
    schema: &TableSchema,
    existing: &[Option<ScalarRef<'_>>],
    overrides: &[(usize, &Value)],
) -> Result<Vec<u8>> {
    if existing.len() > schema.columns.len() {
        return Err(RowCodecError::Invalid(format!(
            "too many existing values: schema has {} columns, got {}",
            schema.columns.len(),
            existing.len()
        )));
    }

    // 累計上限検証・確保前 reserve は encode_scalar_columns と同一（コメントは
    // 重複させず同関数を参照）。
    let mut buf = Vec::new();
    let mut total_len: u32 = 0;
    let mut reserve = |buf: &mut Vec<u8>, additional: u32| -> Result<()> {
        total_len = total_len
            .checked_add(additional)
            .ok_or_else(|| RowCodecError::Invalid("scalar payload length overflow".to_string()))?;
        if total_len > MAX_SCALAR_PAYLOAD_LEN {
            return Err(RowCodecError::Invalid(format!(
                "scalar payload length {total_len} exceeds limit {MAX_SCALAR_PAYLOAD_LEN}"
            )));
        }
        if buf.capacity() < total_len as usize {
            let needed = (total_len as usize).saturating_sub(buf.len());
            buf.try_reserve_exact(needed).map_err(|_| {
                RowCodecError::Invalid("failed to reserve scalar payload buffer".to_string())
            })?;
        }
        Ok(())
    };

    let write_text = |buf: &mut Vec<u8>,
                      reserve: &mut dyn FnMut(&mut Vec<u8>, u32) -> Result<()>,
                      text_bytes: &[u8]|
     -> Result<()> {
        let text_len = u32::try_from(text_bytes.len()).map_err(|_| {
            RowCodecError::Invalid(format!("text field too long: {} bytes", text_bytes.len()))
        })?;
        if text_len > MAX_TEXT_FIELD_LEN {
            return Err(RowCodecError::Invalid(format!(
                "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
            )));
        }
        let entry_len = scalar_text_entry_len(text_len)?;
        reserve(buf, entry_len)?;
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&text_len.to_le_bytes());
        buf.extend_from_slice(text_bytes);
        Ok(())
    };

    let write_array = |buf: &mut Vec<u8>,
                       reserve: &mut dyn FnMut(&mut Vec<u8>, u32) -> Result<()>,
                       array_ty: ArrayType,
                       array_value: &ArrayValue|
     -> Result<()> {
        let entry_len = scalar_array_entry_len(array_ty.elem(), array_value)?;
        reserve(buf, entry_len)?;
        buf.push(PRESENCE_VALUE);
        write_array_value(buf, array_ty, array_value)
    };

    // BYTEA 用の長さプレフィックス書き込み。フレーミングは `write_text` と
    // 完全に同一（presence(1) + 長さ(4) + 本体）だが、UTF-8 検証を行わず
    // 上限を `MAX_BYTEA_FIELD_LEN` で検証する点のみ異なる（B3）。
    let write_bytes = |buf: &mut Vec<u8>,
                       reserve: &mut dyn FnMut(&mut Vec<u8>, u32) -> Result<()>,
                       bytes: &[u8]|
     -> Result<()> {
        let byte_len = u32::try_from(bytes.len()).map_err(|_| {
            RowCodecError::Invalid(format!("bytea field too long: {} bytes", bytes.len()))
        })?;
        if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
            return Err(RowCodecError::Invalid(format!(
                "bytea field length {byte_len} exceeds limit {}",
                crate::bytea::MAX_BYTEA_FIELD_LEN
            )));
        }
        let entry_len = scalar_text_entry_len(byte_len)?;
        reserve(buf, entry_len)?;
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&byte_len.to_le_bytes());
        buf.extend_from_slice(bytes);
        Ok(())
    };

    // JSON／JSONB 用の長さプレフィックス書き込み。フレーミングは `write_text` と
    // 完全に同一で、列型に応じた検証（`validate_json_column_value`）のみ異なる。
    let write_json = |buf: &mut Vec<u8>,
                      reserve: &mut dyn FnMut(&mut Vec<u8>, u32) -> Result<()>,
                      column: &crate::catalog::ColumnDef,
                      text: &str|
     -> Result<()> {
        validate_json_column_value(column, text)?;
        write_text(buf, reserve, text.as_bytes())
    };

    for slot in schema.physical_slots() {
        let (idx, column) = match slot {
            crate::catalog::PhysicalSlot::Live(idx, column) => (idx, column),
            crate::catalog::PhysicalSlot::Dropped(_) => {
                // 削除済み列（墓標）は UPDATE でも常に NULL を書く。`existing`
                // （削除前の値。構造検証のみ経由し出力へは積まれていない）・
                // `overrides` のいずれも参照しない（削除済みの値を引き継がない。
                // TABLE-19 D1・Issue #901）。
                reserve(&mut buf, 1)?;
                buf.push(PRESENCE_NULL);
                continue;
            }
        };
        if matches!(column.ty, ColumnType::Vector(_)) {
            // VECTOR 列は encode_scalar_columns と同じくスキップ（embedding は
            // storage.rs 側のスロットが担当。`overrides` に含まれていても無視）。
            continue;
        }
        if let Some((_, value)) = overrides.iter().find(|(o_idx, _)| *o_idx == idx) {
            // SET 対象列（クライアント入力）を書き込む。encode_scalar_columns と
            // 同じ検証・書式。
            match value {
                Value::Null => {
                    if !column.nullable {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} is not nullable but value is missing",
                            column.name
                        )));
                    }
                    reserve(&mut buf, 1)?;
                    buf.push(PRESENCE_NULL);
                }
                Value::Text(text) => {
                    if !matches!(column.ty, ColumnType::Text) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Text value, got Text",
                            column.name
                        )));
                    }
                    write_text(&mut buf, &mut reserve, text.as_bytes())?;
                }
                Value::Real(v) => {
                    if !matches!(column.ty, ColumnType::Real) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Real value, got Real",
                            column.name
                        )));
                    }
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: REAL value must be finite",
                            column.name
                        )));
                    }
                    // F4: -0.0 正規化。
                    let v = crate::scalar_float::canonicalize_real(*v);
                    reserve(&mut buf, SCALAR_REAL_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Value::Double(v) => {
                    if !matches!(column.ty, ColumnType::Double) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Double value, got Double",
                            column.name
                        )));
                    }
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: DOUBLE PRECISION value must be finite",
                            column.name
                        )));
                    }
                    // F4: -0.0 正規化。
                    let v = crate::scalar_float::canonicalize_double(*v);
                    reserve(&mut buf, SCALAR_DOUBLE_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Value::Enum(label) => {
                    let def = match &column.ty {
                        ColumnType::Enum(def) => def,
                        ColumnType::Text
                        | ColumnType::Integer
                        | ColumnType::BigInt
                        | ColumnType::Vector(_)
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
                            return Err(RowCodecError::Invalid(format!(
                                "column {:?} expects a non-Enum value, got Enum",
                                column.name
                            )))
                        }
                    };
                    if def.validate_label(label).is_err() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} does not accept label {label:?} for enum type {:?}",
                            column.name,
                            def.name()
                        )));
                    }
                    write_text(&mut buf, &mut reserve, label.as_bytes())?;
                }
                Value::Vector(_) => {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} expects a non-Vector value, got Vector",
                        column.name
                    )))
                }
                Value::Integer(v) => {
                    if !matches!(column.ty, ColumnType::Integer) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Integer value, got Integer",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_INT4_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Value::BigInt(v) => {
                    if !matches!(column.ty, ColumnType::BigInt) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-BigInt value, got BigInt",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_INT8_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Value::Bool(b) => {
                    if !matches!(column.ty, ColumnType::Boolean) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Boolean value, got Boolean",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_BOOL_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.push(if *b { BOOL_TRUE_BYTE } else { BOOL_FALSE_BYTE });
                }
                Value::Date(days) => {
                    if !matches!(column.ty, ColumnType::Date) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Date value, got Date",
                            column.name
                        )));
                    }
                    if !crate::datetime::validate_date_days(*days) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: date value out of range: {days}",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_DATE_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&days.to_le_bytes());
                }
                Value::Timestamp(micros) => {
                    if !matches!(column.ty, ColumnType::Timestamp) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Timestamp value, got Timestamp",
                            column.name
                        )));
                    }
                    if !crate::datetime::validate_timestamp_micros(*micros) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: timestamp value out of range: {micros}",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_TIMESTAMP_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&micros.to_le_bytes());
                }
                Value::Array(array_value) => {
                    let array_ty = match &column.ty {
                        ColumnType::Array(array_ty) => *array_ty,
                        ColumnType::Text
                        | ColumnType::Integer
                        | ColumnType::BigInt
                        | ColumnType::Vector(_)
                        | ColumnType::Real
                        | ColumnType::Double
                        | ColumnType::Boolean
                        | ColumnType::Date
                        | ColumnType::Timestamp
                        | ColumnType::Bytea
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        | ColumnType::Enum(_)
                        | ColumnType::Numeric { .. }
                        | ColumnType::Uuid => {
                            return Err(RowCodecError::Invalid(format!(
                                "column {:?} expects a non-Array value, got Array",
                                column.name
                            )))
                        }
                    };
                    write_array(&mut buf, &mut reserve, array_ty, array_value)?;
                }
                Value::Bytes(bytes) => {
                    if !matches!(column.ty, ColumnType::Bytea) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Bytea value, got Bytea",
                            column.name
                        )));
                    }
                    write_bytes(&mut buf, &mut reserve, bytes)?;
                }
                Value::Json(text) => {
                    write_json(&mut buf, &mut reserve, column, text)?;
                }
                Value::Numeric(d) => {
                    let (precision, scale) = match column.ty {
                        ColumnType::Numeric { precision, scale } => (precision, scale),
                        ColumnType::Text
                        | ColumnType::Integer
                        | ColumnType::BigInt
                        | ColumnType::Real
                        | ColumnType::Double
                        | ColumnType::Vector(_)
                        | ColumnType::Boolean
                        | ColumnType::Date
                        | ColumnType::Timestamp
                        | ColumnType::Array(_)
                        | ColumnType::Bytea
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        | ColumnType::Enum(_)
                        | ColumnType::Uuid => {
                            return Err(RowCodecError::Invalid(format!(
                                "column {:?} expects a non-Numeric value, got Numeric",
                                column.name
                            )))
                        }
                    };
                    if d.scale() != scale || !d.fits_precision(precision) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} numeric value does not match column NUMERIC({precision},{scale})",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_NUMERIC_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&d.unscaled().to_le_bytes());
                }
                Value::Uuid(u) => {
                    if !matches!(column.ty, ColumnType::Uuid) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Uuid value, got Uuid",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_UUID_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(u.as_bytes());
                }
            }
        } else {
            // SET 対象でない列は既存の借用値（またはNULL）をそのまま書き込む。
            // `existing` は `scan_scalar_columns` の契約により non-nullable 列で
            // `None` になり得ないが（構造検証済み）、untrusted な格納済みデータに
            // 由来する不変条件のため呼び出し元契約が破れた場合も fail-closed に
            // 拒否する。`ScalarRef` の型が列型と一致しない場合（呼び出し元契約が
            // 破れた場合）も同様に fail-closed に拒否する（Issue #881 D3）。
            match existing.get(idx).copied().flatten() {
                None => {
                    if !column.nullable {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} is not nullable but existing value is missing",
                            column.name
                        )));
                    }
                    reserve(&mut buf, 1)?;
                    buf.push(PRESENCE_NULL);
                }
                Some(ScalarRef::Text(text)) => {
                    if !matches!(column.ty, ColumnType::Text) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Text existing value, got Text",
                            column.name
                        )));
                    }
                    write_text(&mut buf, &mut reserve, text.as_bytes())?;
                }
                Some(ScalarRef::Integer(v)) => {
                    if !matches!(column.ty, ColumnType::Integer) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Integer existing value, got Integer",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_INT4_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Some(ScalarRef::BigInt(v)) => {
                    if !matches!(column.ty, ColumnType::BigInt) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-BigInt existing value, got BigInt",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_INT8_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Some(ScalarRef::Real(v)) => {
                    // 既存の永続バイトから借用した値。decode 側で非有限値は
                    // 既に拒否済みのため有限性は保証されるが、untrusted な
                    // 格納済みデータに由来する不変条件のため念のため再検証する。
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: existing REAL value must be finite",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_REAL_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                Some(ScalarRef::Double(v)) => {
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: existing DOUBLE PRECISION value must be finite",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_DOUBLE_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                // 既存の ENUM 値をそのまま再書き込みする（SET 対象でない列）。
                // 既に格納済みの値であり、語彙は書き込み時（encode_row 系）に
                // 一度検査済みのため、ここで再検査する必要はない（Issue #890 D3。
                // decode 側は語彙を検査しない契約と対称）。
                Some(ScalarRef::Enum(label)) => {
                    write_text(&mut buf, &mut reserve, label.as_bytes())?;
                }
                Some(ScalarRef::Bool(b)) => {
                    if !matches!(column.ty, ColumnType::Boolean) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} expects a non-Boolean existing value, got Boolean",
                            column.name
                        )));
                    }
                    reserve(&mut buf, SCALAR_BOOL_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.push(if b { BOOL_TRUE_BYTE } else { BOOL_FALSE_BYTE });
                }
                Some(ScalarRef::Date(days)) => {
                    reserve(&mut buf, SCALAR_DATE_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&days.to_le_bytes());
                }
                Some(ScalarRef::Timestamp(micros)) => {
                    reserve(&mut buf, SCALAR_TIMESTAMP_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&micros.to_le_bytes());
                }
                Some(ScalarRef::Array(array_ref)) => {
                    let payload_len = u32::try_from(array_ref.bytes.len()).map_err(|_| {
                        RowCodecError::Invalid("array payload length overflow".to_string())
                    })?;
                    // presence(1) + flags(1) + count(4) + payload_len(4) + 本文。
                    let entry_len = 10u32.checked_add(payload_len).ok_or_else(|| {
                        RowCodecError::Invalid("array entry length overflow".to_string())
                    })?;
                    reserve(&mut buf, entry_len)?;
                    buf.push(PRESENCE_VALUE);
                    write_array_ref(&mut buf, &array_ref)?;
                }
                Some(ScalarRef::Bytes(bytes)) => {
                    write_bytes(&mut buf, &mut reserve, bytes)?;
                }
                Some(ScalarRef::Json(text)) => {
                    // 既存値は encode 済みで格納契約（構文検証・JSONB は正規化）を
                    // 満たしている前提のため、TEXT／BYTEA の既存値と同じく
                    // 再パース・再検証はしない（decode 契約「書き込み経路の保証に
                    // 依拠」に揃える。Issue #889 D2。`write_canonical` の出力形式
                    // が将来変わっても、SET 対象でない既存 JSONB 行が
                    // `XX000` で更新不能になるフォワード互換の結合を避ける）。
                    write_text(&mut buf, &mut reserve, text.as_bytes())?;
                }
                Some(ScalarRef::Numeric(d)) => {
                    reserve(&mut buf, SCALAR_NUMERIC_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(&d.unscaled().to_le_bytes());
                }
                Some(ScalarRef::Uuid(u)) => {
                    reserve(&mut buf, SCALAR_UUID_ENTRY_LEN)?;
                    buf.push(PRESENCE_VALUE);
                    buf.extend_from_slice(u.as_bytes());
                }
            }
        }
    }

    Ok(buf)
}

/// [`encode_scalar_columns`] の構造検証のみを行う borrow 版パーサー（Issue #56
/// レビュー指摘対応・codex P1: `sql::exec::execute_statement` の `on_visible_row` は
/// RLS/SCALAR フィルタ列・投影に不要な列も含め毎行デコードするが、旧
/// `decode_scalar_columns` は全 `Text` 列を無条件に `to_string()` で確保していたため、
/// 最大長 `Text` 列を多数持つスキーマでは投影が不要とする列も含め 1 行分の巨大な
/// 一時確保が発生し得た（`.claude/rules/security.md`「不安全な設計｜無制限リソース
/// 確保（DoS）」）。本関数は presence タグ・宣言長（[`MAX_TEXT_FIELD_LEN`] 超過）・
/// UTF-8 妥当性をすべて検証しつつ、`Text` 値は `buf` を借用した `&str` として返し、
/// 一切ヒープ確保しない。戻り値は `schema.columns` と同じ長さ・順序を持ち、
/// `VECTOR` 列・`NULL` 列の位置は常に `None`。エラー種別は [`decode_row`] と同じ規則
/// （presence タグ不正・宣言長が残りバッファを超える・末尾に余剰バイトがある場合は
/// `Err`。バッファ末尾で打ち切られた nullable 列は欠落として許容）。
///
/// 呼び出し元は、フィルタ条件の突合には借用のまま使い（確保不要）、実際に投影・
/// hybrid 本文として必要な列だけを [`Value::Text`] へ選択的に複製する
/// （`sql::exec.rs` の `on_visible_row` 参照。累計・行単位の確保量上限はそちらが
/// アロケーション前に検証する）。
pub fn scan_scalar_columns<'a>(
    schema: &'a TableSchema,
    buf: &'a [u8],
) -> Result<Vec<Option<ScalarRef<'a>>>> {
    scan_scalar_columns_masked(schema, buf, None)
}

/// [`scan_scalar_columns`] の列限定版（Issue #350: 集計経路が実際に参照しない列の
/// `&str` 生成コストを避けるための必要列限定デコード）。`mask` が `Some(m)` のとき
/// `m[i] == false` の列も構造検証（presence タグ・宣言長上限 [`MAX_TEXT_FIELD_LEN`]・
/// バッファ境界）に加え UTF-8 妥当性検証まで常に行い（codex-review P1 指摘・PR #369:
/// 未参照列でも不正 UTF-8 を含む永続行を fail-closed で拒否する既存のエラー契約を
/// 維持する必要があるため）、検証済みの `&str` の生成・保持のみを省略して常に `None`
/// を積む（untrusted な行バッファに対する検証はマスク外の列でも一切弱めない。
/// `.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。`mask` が `None`
/// （[`scan_scalar_columns`] 経由）は従来どおり全列を `&str` 化する。
/// `mask.len() != schema.columns.len()` は fail-closed に `Err` とし、呼び出し元の
/// 列インデックス計算の誤りを黙って無視しない。
pub fn scan_scalar_columns_masked<'a>(
    schema: &'a TableSchema,
    buf: &'a [u8],
    mask: Option<&[bool]>,
) -> Result<Vec<Option<ScalarRef<'a>>>> {
    let mut values: Vec<Option<ScalarRef<'a>>> = Vec::new();
    values
        .try_reserve_exact(schema.columns.len())
        .map_err(|_| RowCodecError::Invalid("failed to reserve scalar scan output".to_string()))?;
    scan_scalar_columns_validated(schema, buf, mask, |col_index, value| {
        // 削除済み列（墓標。`col_index == None`）は物理走査上の構造検証のみを
        // 行い、出力へは一切積まない（`values` は常に論理列数と同じ長さを保つ。
        // TABLE-19・Issue #901）。
        if col_index.is_some() {
            values.push(value);
        }
        Ok(())
    })?;
    Ok(values)
}

/// [`scan_scalar_columns_masked`] の検証専用版（codex-review P1 指摘対応・PR #369:
/// `sql::aggregate::DecodeTier::Fast` は `scalar_mask` が全列 `false`
/// （`COUNT(*)`・`SUM(id)` 等でどのスカラー列も参照しない）のときに使う tier だが、
/// 従来は `scan_scalar_columns_masked` を呼び `Vec<Option<&str>>` を毎行確保しており、
/// `docs/design/aggregate-decode-skip.md` が定める「`Fast` は結果 Vec 生成を省略する」
/// という tier 契約に反していた）。presence タグ・宣言長上限
/// （[`MAX_TEXT_FIELD_LEN`]）・バッファ境界・UTF-8 妥当性の構造検証は
/// [`scan_scalar_columns_masked`] と完全に同じ経路（[`scan_scalar_columns_validated`]）
/// を通り一切弱めない（untrusted な行バッファへの fail-closed 契約を維持。
/// `.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。検証済みの値を
/// 呼び出し元へ返さない・保持しないため `Vec` を一切確保しない。
pub fn validate_scalar_columns(schema: &TableSchema, buf: &[u8]) -> Result<()> {
    scan_scalar_columns_validated(schema, buf, None, |_, _| Ok(()))
}

/// [`scan_scalar_columns_validated`] が生存列・削除済み列（墓標）のいずれについても
/// 同じ型別ディスパッチ本体を共有するための薄い借用ビュー（TABLE-19・Issue #901）。
/// 生存列は `&ColumnDef` からそのまま、墓標は `&DroppedSlot`（常に nullable 扱い。
/// TABLE-19 D1）から構築する。フィールド名を `ColumnDef` と揃えているため、
/// 本体側のコードは `column.name`／`column.ty`／`column.nullable` を無変更で
/// 参照できる。
struct ScalarSlotView<'a> {
    name: &'a str,
    ty: &'a ColumnType,
    nullable: bool,
    /// 生存列の `DEFAULT`。バッファ末尾で欠落した列（`ALTER TABLE ADD COLUMN`
    /// 後の既存行）を読み出し時に補う値の出どころ（Issue #1169）。墓標は常に `None`。
    default: Option<&'a crate::catalog::ColumnDefault>,
}

impl<'a> From<&'a crate::catalog::ColumnDef> for ScalarSlotView<'a> {
    fn from(c: &'a crate::catalog::ColumnDef) -> Self {
        Self {
            name: c.name.as_str(),
            ty: &c.ty,
            nullable: c.nullable,
            default: c.default.as_ref(),
        }
    }
}

impl<'a> From<&'a crate::catalog::DroppedSlot> for ScalarSlotView<'a> {
    fn from(d: &'a crate::catalog::DroppedSlot) -> Self {
        Self {
            name: d.name(),
            ty: d.ty(),
            // 削除済み列は元の nullable 宣言に関わらず常に nullable 扱いとする
            // （削除前は non-nullable でも、削除後の新規行は当該位置を常に
            // NULL として書くため。TABLE-19 D1）。
            nullable: true,
            default: None,
        }
    }
}

/// [`column_default_scalar`] の失敗種別。SQL 表層の SQLSTATE 写像
/// （`sql::parser::bind_column_default`）とカタログ層の事前検証
/// （`catalog::Storage::alter_table_add_column`）が共有する。値・列名は保持しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultBindError {
    /// 列型とリテラル種別が合わない（大分類の不整合・DEFAULT 非対応型）。
    Incompatible,
    /// リテラルの文法が不正（`UUID` の文字列不正を含む。詳細は保持しない）。
    Malformed,
    /// 列型の値域外（`INTEGER` オーバーフロー・`NUMERIC` 桁数超過等）。
    OutOfRange,
    /// `DATE`／`TIMESTAMP` リテラルの書式違反（SQLSTATE `22007`。Issue #1279・#1280）。
    DatetimeFormat,
    /// `DATE`／`TIMESTAMP` リテラルの範囲外・暦上不正（SQLSTATE `22008`。Issue #1279・#1280）。
    DatetimeOverflow,
    /// `ENUM` の既定値が語彙外のラベル（SQLSTATE `22P02`。Issue #1282）。
    EnumLabel,
}

/// 列の `DEFAULT` を型付きスカラー値へ変換する唯一の実装（Issue #1169）。
/// `INSERT` の列省略補完（`sql::parser::bind_column_default` 経由）と、
/// `ALTER TABLE ADD COLUMN` 後の既存行の読み出し時補完
/// （[`scan_scalar_columns_validated`]）が同じ関数を通り、値の食い違いを構造的に
/// 排除する。`Text` は `column` を借用しゼロコピー。`default == None` は `Ok(None)`。
/// 列の `DEFAULT` は作成後に変わらない（`ALTER COLUMN SET DEFAULT` は存在しない）
/// ことが読み出し時補完の前提で、将来導入する場合は補完専用の値を分離する必要がある。
pub(crate) fn column_default_scalar(
    column: &crate::catalog::ColumnDef,
) -> std::result::Result<Option<ScalarRef<'_>>, DefaultBindError> {
    match &column.default {
        None => Ok(None),
        Some(default) => default_scalar(&column.ty, default).map(Some),
    }
}

/// [`column_default_scalar`] の本体（型と既定値を直接受ける版）。
pub(crate) fn default_scalar<'a>(
    ty: &ColumnType,
    default: &'a crate::catalog::ColumnDefault,
) -> std::result::Result<ScalarRef<'a>, DefaultBindError> {
    use crate::catalog::ColumnDefault;
    fn int_err(e: &std::num::ParseIntError) -> DefaultBindError {
        match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                DefaultBindError::OutOfRange
            }
            _ => DefaultBindError::Malformed,
        }
    }
    fn float_err(e: crate::scalar_float::ParseFloatError) -> DefaultBindError {
        match e {
            crate::scalar_float::ParseFloatError::Malformed => DefaultBindError::Malformed,
            crate::scalar_float::ParseFloatError::OutOfRange => DefaultBindError::OutOfRange,
        }
    }
    match (default, ty) {
        (ColumnDefault::Text(s), ColumnType::Text) => Ok(ScalarRef::Text(s.as_str())),
        (ColumnDefault::Number(n), ColumnType::Integer) => n
            .parse::<i32>()
            .map(ScalarRef::Integer)
            .map_err(|e| int_err(&e)),
        (ColumnDefault::Number(n), ColumnType::BigInt) => n
            .parse::<i64>()
            .map(ScalarRef::BigInt)
            .map_err(|e| int_err(&e)),
        (ColumnDefault::Number(n), ColumnType::Real) => crate::scalar_float::parse_real(n)
            .map(ScalarRef::Real)
            .map_err(float_err),
        (ColumnDefault::Number(n), ColumnType::Double) => crate::scalar_float::parse_double(n)
            .map(ScalarRef::Double)
            .map_err(float_err),
        (ColumnDefault::Number(n), ColumnType::Numeric { precision, scale }) => {
            match crate::numeric::parse_for_column(n, *precision, *scale) {
                Ok(d) => Ok(ScalarRef::Numeric(d)),
                Err(crate::numeric::NumericError::Malformed(_)) => Err(DefaultBindError::Malformed),
                Err(crate::numeric::NumericError::OutOfRange) => Err(DefaultBindError::OutOfRange),
            }
        }
        (ColumnDefault::Bool(b), ColumnType::Boolean) => Ok(ScalarRef::Bool(*b)),
        // `DATE` 列の既定値は文字列リテラルを `datetime::parse_date`（INSERT の
        // リテラル束縛と同じ閉じた文法）で解釈する（Issue #1279）。
        (ColumnDefault::Text(s), ColumnType::Date) => match crate::datetime::parse_date(s) {
            Ok(days) => Ok(ScalarRef::Date(days)),
            Err(crate::datetime::DateTimeLiteralError::Format(_)) => {
                Err(DefaultBindError::DatetimeFormat)
            }
            Err(crate::datetime::DateTimeLiteralError::Overflow(_)) => {
                Err(DefaultBindError::DatetimeOverflow)
            }
        },
        // `ENUM` 列の既定値は文字列リテラルの原文を、カタログ登録済みの語彙
        // （`EnumTypeDef::contains`。語彙検証の単一情報源）で照合する（Issue #1282）。
        // `ScalarRef::Text` ではなく `ScalarRef::Enum` を返し、下流（encode の型ガード・
        // 等価フィルタ・二次索引）が ENUM として一貫して扱えるようにする。
        (ColumnDefault::Text(s), ColumnType::Enum(def)) => {
            if def.contains(s) {
                Ok(ScalarRef::Enum(s.as_str()))
            } else {
                Err(DefaultBindError::EnumLabel)
            }
        }
        // `UUID` 列の既定値は文字列リテラルを `uuid::parse_uuid_text`（INSERT の
        // `bind_uuid_literal` と同じ厳密文法）で解釈する（Issue #1281）。文法不正は
        // `Malformed`（22P02）。原文のまま保持し、読み出し時は正規小文字表記になる。
        (ColumnDefault::Text(s), ColumnType::Uuid) => crate::uuid::parse_uuid_text(s)
            .map(ScalarRef::Uuid)
            .map_err(|_| DefaultBindError::Malformed),
        // `TIMESTAMP` 列も同様に `datetime::parse_timestamp`（INSERT のリテラル束縛と
        // 同じ閉じた文法。揮発性の式は受理しない）で解釈する（Issue #1280）。
        (ColumnDefault::Text(s), ColumnType::Timestamp) => {
            match crate::datetime::parse_timestamp(s) {
                Ok(micros) => Ok(ScalarRef::Timestamp(micros)),
                Err(crate::datetime::DateTimeLiteralError::Format(_)) => {
                    Err(DefaultBindError::DatetimeFormat)
                }
                Err(crate::datetime::DateTimeLiteralError::Overflow(_)) => {
                    Err(DefaultBindError::DatetimeOverflow)
                }
            }
        }
        _ => Err(DefaultBindError::Incompatible),
    }
}

/// [`scan_scalar_columns_masked`]・[`validate_scalar_columns`] が共有する走査本体。
/// 列ごとの構造検証（presence タグ・宣言長上限・バッファ境界・UTF-8 妥当性）を
/// 一箇所に集約し、検証済みの値（`col_index`・`Option<&'a str>`）を `sink` へ渡す
/// だけで、値の保持要否（`Vec` へ積むか捨てるか）は呼び出し元が選ぶ。`mask` の意味
/// は [`scan_scalar_columns_masked`] のドキュメントコメントを参照
/// （`m[i] == false` の列も検証は行い、`sink` へは `None` を渡す）。
///
/// 走査は論理列（`schema.columns`）ではなく物理配置
/// （[`TableSchema::physical_slots`]）に沿って行う（TABLE-19・Issue #901）。
/// 削除済み列（墓標）は行バイト列上の位置を占有し続けるため、後続の生存列を
/// 正しくデコードするには墓標の型に応じたバイト数を読み飛ばす必要がある一方、
/// 墓標自体は論理列を持たないため `sink` には `col_index = None` で渡され、
/// 呼び出し元は出力へ積まない（構造検証のみ行い読み捨てる）。
fn scan_scalar_columns_validated<'a>(
    schema: &'a TableSchema,
    buf: &'a [u8],
    mask: Option<&[bool]>,
    mut sink: impl FnMut(Option<usize>, Option<ScalarRef<'a>>) -> Result<()>,
) -> Result<()> {
    if let Some(m) = mask {
        if m.len() != schema.columns.len() {
            return Err(RowCodecError::Invalid(
                "scalar column mask length does not match schema column count".to_string(),
            ));
        }
    }
    let mut offset = 0usize;
    for slot in schema.physical_slots() {
        let (col_index, column): (Option<usize>, ScalarSlotView<'_>) = match slot {
            crate::catalog::PhysicalSlot::Live(logical_index, c) => {
                (Some(logical_index), ScalarSlotView::from(c))
            }
            crate::catalog::PhysicalSlot::Dropped(d) => (None, ScalarSlotView::from(d)),
        };
        if matches!(column.ty, ColumnType::Vector(_)) {
            sink(col_index, None)?;
            continue;
        }
        let wanted = col_index
            .map(|i| mask.map(|m| m[i]).unwrap_or(true))
            .unwrap_or(false);
        let presence = match buf.get(offset) {
            Some(&b) => b,
            None => {
                // バッファ末尾で欠落した列（`ALTER TABLE ADD COLUMN` 後の既存行。
                // TABLE-5・TABLE-16、Issue #1169）。生存列で `DEFAULT` があれば
                // 読み出し時に既定値を補い（行は書き換えない）、なければ nullable
                // のみ NULL として許容する。墓標は `default == None` のため従来どおり。
                if let Some(default) = column.default {
                    // マスク外でも DEFAULT の妥当性は検証する（参照列の選択で
                    // カタログ破損の検出結果が変わらないようにする。fail-closed）。
                    // 出力の保持だけを `wanted` で省略する。
                    let value = default_scalar(column.ty, default).map_err(|_| {
                        RowCodecError::Invalid(format!(
                            "column {:?} has an invalid DEFAULT for its type",
                            column.name
                        ))
                    })?;
                    if wanted {
                        sink(col_index, Some(value))?;
                    } else {
                        sink(col_index, None)?;
                    }
                    continue;
                }
                if column.nullable {
                    sink(col_index, None)?;
                    continue;
                } else {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but scalar payload is truncated",
                        column.name
                    )));
                }
            }
        };
        offset = offset.checked_add(1).ok_or_else(|| {
            RowCodecError::Invalid("offset overflow after presence field".to_string())
        })?;

        match presence {
            PRESENCE_NULL => {
                if !column.nullable {
                    return Err(RowCodecError::Invalid(format!(
                        "column {:?} is not nullable but value is NULL",
                        column.name
                    )));
                }
                sink(col_index, None)?;
            }
            PRESENCE_VALUE => match column.ty {
                ColumnType::Real => {
                    let bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before real field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at real field".to_string(),
                            )
                        })?;
                    let arr: [u8; 4] = bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("real field is not 4 bytes".to_string())
                    })?;
                    let v = f32::from_le_bytes(arr);
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: persisted REAL value is not finite",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after real field".to_string())
                    })?;
                    let v = crate::scalar_float::canonicalize_real(v);
                    if wanted {
                        sink(col_index, Some(ScalarRef::Real(v)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Double => {
                    let bytes = buf
                        .get(
                            offset..offset.checked_add(8).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before double field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at double field".to_string(),
                            )
                        })?;
                    let arr: [u8; 8] = bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("double field is not 8 bytes".to_string())
                    })?;
                    let v = f64::from_le_bytes(arr);
                    if !v.is_finite() {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?}: persisted DOUBLE PRECISION value is not finite",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after double field".to_string())
                    })?;
                    let v = crate::scalar_float::canonicalize_double(v);
                    if wanted {
                        sink(col_index, Some(ScalarRef::Double(v)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Boolean => {
                    let byte = *buf.get(offset).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "scalar payload truncated at boolean value field".to_string(),
                        )
                    })?;
                    let b = match byte {
                        BOOL_FALSE_BYTE => false,
                        BOOL_TRUE_BYTE => true,
                        other => {
                            return Err(RowCodecError::Invalid(format!(
                                "unknown boolean value byte: {other}"
                            )))
                        }
                    };
                    offset = offset.checked_add(1).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after boolean value field".to_string(),
                        )
                    })?;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Bool(b)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Numeric { precision, scale } => {
                    let unscaled_bytes = buf
                        .get(
                            offset..offset.checked_add(16).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before numeric value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at numeric value field".to_string(),
                            )
                        })?;
                    let unscaled_arr: [u8; 16] = unscaled_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("numeric value field is not 16 bytes".to_string())
                    })?;
                    let unscaled = i128::from_le_bytes(unscaled_arr);
                    let decimal = Decimal::from_parts(unscaled, *scale).map_err(|_| {
                        RowCodecError::Invalid(format!(
                            "column {:?} numeric scale out of range",
                            column.name
                        ))
                    })?;
                    if !decimal.fits_precision(*precision) {
                        return Err(RowCodecError::Invalid(format!(
                            "column {:?} numeric value out of range for precision {precision}",
                            column.name
                        )));
                    }
                    offset = offset.checked_add(16).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after numeric value field".to_string(),
                        )
                    })?;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Numeric(decimal)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Uuid => {
                    let uuid_bytes: [u8; 16] = buf
                        .get(
                            offset..offset.checked_add(16).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before uuid value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at uuid value field".to_string(),
                            )
                        })?
                        .try_into()
                        .map_err(|_| {
                            RowCodecError::Invalid("uuid value field is not 16 bytes".to_string())
                        })?;
                    offset = offset.checked_add(16).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after uuid value field".to_string())
                    })?;
                    if wanted {
                        sink(
                            col_index,
                            Some(ScalarRef::Uuid(Uuid::from_bytes(uuid_bytes))),
                        )?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Text | ColumnType::Vector(_) | ColumnType::Enum(_) => {
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before text length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at text length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("text length field is not 4 bytes".to_string())
                    })?;
                    let text_len = u32::from_le_bytes(len_arr);
                    if text_len > MAX_TEXT_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "text field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after text length field".to_string(),
                        )
                    })?;
                    let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after text field".to_string())
                    })?;
                    let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                        RowCodecError::Invalid("scalar payload truncated at text field".to_string())
                    })?;
                    offset = text_end;
                    // UTF-8 妥当性検証は要求列・非要求列を問わず常に行う（codex-review
                    // P1 指摘・PR #369: 未参照列でも不正 UTF-8 を含む永続行を fail-closed
                    // で拒否する既存のエラー契約（`XX000`）を維持する必要があるため）。
                    // マスクで要求されなかった列（`validate_scalar_columns` 経由は常に
                    // 全列非要求）は、検証済みの `&str` を破棄して `None` を渡すことで
                    // `&str` 生成・保持コストのみを省略する（Issue #350: 必要列限定
                    // デコード）。ENUM 列は要求・非要求を問わず現行語彙との照合を行う
                    // （codex-review P1 指摘・Issue #890。`decode_row` の検査と同一
                    // 契約。破損行の語彙外ラベルが投影・等価フィルタ・二次索引へ
                    // 流出するのを構造検証のみのマスク非要求経路でも防ぐ）。
                    let text = std::str::from_utf8(text_bytes).map_err(|_| {
                        RowCodecError::Invalid("text field is not valid UTF-8".to_string())
                    })?;
                    if let ColumnType::Enum(def) = column.ty {
                        if !def.contains(text) {
                            return Err(RowCodecError::Invalid(format!(
                                "enum value {text:?} is not a valid label of type {:?}",
                                def.name()
                            )));
                        }
                        if wanted {
                            sink(col_index, Some(ScalarRef::Enum(text)))?;
                        } else {
                            sink(col_index, None)?;
                        }
                    } else if wanted {
                        sink(col_index, Some(ScalarRef::Text(text)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Integer => {
                    let field_end = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after integer field".to_string())
                    })?;
                    let field_bytes = buf.get(offset..field_end).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "scalar payload truncated at integer field".to_string(),
                        )
                    })?;
                    let arr: [u8; 4] = field_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("integer field is not 4 bytes".to_string())
                    })?;
                    offset = field_end;
                    let v = i32::from_le_bytes(arr);
                    if wanted {
                        sink(col_index, Some(ScalarRef::Integer(v)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::BigInt => {
                    let field_end = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after bigint field".to_string())
                    })?;
                    let field_bytes = buf.get(offset..field_end).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "scalar payload truncated at bigint field".to_string(),
                        )
                    })?;
                    let arr: [u8; 8] = field_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("bigint field is not 8 bytes".to_string())
                    })?;
                    offset = field_end;
                    let v = i64::from_le_bytes(arr);
                    if wanted {
                        sink(col_index, Some(ScalarRef::BigInt(v)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Array(array_ty) => {
                    // 構造・UTF-8・要素上限の検証は要求列・非要求列を問わず
                    // 常に行う（Text 列と同じ方針。`parse_array_frame` が全検証を
                    // 内包する）。
                    let (array_ref, new_offset) =
                        parse_array_frame(buf, offset, array_ty.elem(), array_ty.max_len())?;
                    offset = new_offset;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Array(array_ref)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Bytea => {
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before bytea length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at bytea length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("bytea length field is not 4 bytes".to_string())
                    })?;
                    let byte_len = u32::from_le_bytes(len_arr);
                    if byte_len > crate::bytea::MAX_BYTEA_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "bytea field length {byte_len} exceeds limit {}",
                            crate::bytea::MAX_BYTEA_FIELD_LEN
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after bytea length field".to_string(),
                        )
                    })?;
                    let bytea_end = offset.checked_add(byte_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after bytea field".to_string())
                    })?;
                    let bytea_bytes = buf.get(offset..bytea_end).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "scalar payload truncated at bytea field".to_string(),
                        )
                    })?;
                    offset = bytea_end;
                    // BYTEA は UTF-8 検証を行わない点のみ TEXT と異なる
                    // （presence タグ・宣言長上限・バッファ境界の構造検証は共有）。
                    if wanted {
                        sink(col_index, Some(ScalarRef::Bytes(bytea_bytes)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Date => {
                    let days_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before date value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at date value field".to_string(),
                            )
                        })?;
                    let days_arr: [u8; 4] = days_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("date value field is not 4 bytes".to_string())
                    })?;
                    let days = i32::from_le_bytes(days_arr);
                    if !crate::datetime::validate_date_days(days) {
                        return Err(RowCodecError::Invalid(format!(
                            "date value {days} is out of the representable range"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after date value field".to_string())
                    })?;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Date(days)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Timestamp => {
                    let micros_bytes = buf
                        .get(
                            offset..offset.checked_add(8).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before timestamp value field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at timestamp value field".to_string(),
                            )
                        })?;
                    let micros_arr: [u8; 8] = micros_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("timestamp value field is not 8 bytes".to_string())
                    })?;
                    let micros = i64::from_le_bytes(micros_arr);
                    if !crate::datetime::validate_timestamp_micros(micros) {
                        return Err(RowCodecError::Invalid(format!(
                            "timestamp value {micros} is out of the representable range"
                        )));
                    }
                    offset = offset.checked_add(8).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after timestamp value field".to_string(),
                        )
                    })?;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Timestamp(micros)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
                ColumnType::Json | ColumnType::Jsonb => {
                    // フレーミング・UTF-8 検証は TEXT と共有する（Issue #889 D2）。
                    // 格納時（encode 経路）に構文検証・正規化済みのため、ここでは
                    // 再パースしない（TEXT と同じ契約）。
                    let len_bytes = buf
                        .get(
                            offset..offset.checked_add(4).ok_or_else(|| {
                                RowCodecError::Invalid(
                                    "offset overflow before json length field".to_string(),
                                )
                            })?,
                        )
                        .ok_or_else(|| {
                            RowCodecError::Invalid(
                                "scalar payload truncated at json length field".to_string(),
                            )
                        })?;
                    let len_arr: [u8; 4] = len_bytes.try_into().map_err(|_| {
                        RowCodecError::Invalid("json length field is not 4 bytes".to_string())
                    })?;
                    let text_len = u32::from_le_bytes(len_arr);
                    if text_len > MAX_TEXT_FIELD_LEN {
                        return Err(RowCodecError::Invalid(format!(
                            "json field length {text_len} exceeds limit {MAX_TEXT_FIELD_LEN}"
                        )));
                    }
                    offset = offset.checked_add(4).ok_or_else(|| {
                        RowCodecError::Invalid(
                            "offset overflow after json length field".to_string(),
                        )
                    })?;
                    let text_end = offset.checked_add(text_len as usize).ok_or_else(|| {
                        RowCodecError::Invalid("offset overflow after json field".to_string())
                    })?;
                    let text_bytes = buf.get(offset..text_end).ok_or_else(|| {
                        RowCodecError::Invalid("scalar payload truncated at json field".to_string())
                    })?;
                    offset = text_end;
                    let text = std::str::from_utf8(text_bytes).map_err(|_| {
                        RowCodecError::Invalid("json field is not valid UTF-8".to_string())
                    })?;
                    if wanted {
                        sink(col_index, Some(ScalarRef::Json(text)))?;
                    } else {
                        sink(col_index, None)?;
                    }
                }
            },
            other => {
                return Err(RowCodecError::Invalid(format!(
                    "unknown presence byte: {other}"
                )))
            }
        }
    }

    if offset != buf.len() {
        return Err(RowCodecError::Invalid(
            "scalar payload has trailing bytes beyond declared columns".to_string(),
        ));
    }

    Ok(())
}

/// [`ScalarRef`] の走査結果から `TEXT` 値のみを取り出す（Issue #881 D3）。
/// `declarative_filter::matches_all`／集計・`GROUP BY` の列参照はいずれも
/// 束縛段（`sql/parser.rs`）で `TEXT` 列のみを受理する契約（`INTEGER`／`BIGINT`
/// 列参照は `22000` で拒否済み）のため、実際に参照されるインデックスは常に
/// `ScalarRef::Text` になる。本関数は未参照のまま残る `INTEGER`／`BIGINT` 列を
/// （`VECTOR` 列と同じく）`None` へ落とし、既存の `&[Option<&str>]` 呼び出し規約
/// （`matches_all`・`accumulate_row`・投影の本文列複製）をそのまま再利用できるように
/// する。**この安全性は「束縛段が非 TEXT 列の参照を拒否する」という契約に依存する
/// fail-closed 設計**であり、本関数自体は呼び出し元が渡すインデックスの型を検証
/// しない。`INTEGER`／`BIGINT` 列参照の解禁（Issue #892 の集計・#891 の式評価）が
/// 実装される際は、束縛段の拒否を外す前にこの関数の呼び出し元（あるいは本関数
/// 自体）を見直すこと（黙って `None` に落とすと、値が存在するのに一致しない
/// fail-open へ変わりかねない）。
pub fn scalar_refs_as_text<'a>(scanned: &[Option<ScalarRef<'a>>]) -> Vec<Option<&'a str>> {
    scanned
        .iter()
        .map(|slot| match slot {
            Some(ScalarRef::Text(t)) => Some(*t),
            Some(ScalarRef::Integer(_))
            | Some(ScalarRef::BigInt(_))
            | Some(ScalarRef::Real(_))
            | Some(ScalarRef::Double(_))
            | Some(ScalarRef::Bool(_))
            | Some(ScalarRef::Date(_))
            | Some(ScalarRef::Timestamp(_))
            | Some(ScalarRef::Array(_))
            | Some(ScalarRef::Bytes(_))
            | Some(ScalarRef::Json(_))
            | Some(ScalarRef::Enum(_))
            | Some(ScalarRef::Numeric(_))
            | Some(ScalarRef::Uuid(_))
            | None => None,
        })
        .collect()
}

/// [`encode_scalar_columns`] の逆変換。戻り値は `schema.columns` と同じ長さ・順序を
/// 持ち、`VECTOR` 列の位置は常に `Value::Null`（本関数はその位置のバイトを一切
/// 読み書きしないダミー値。呼び出し元は embedding を `storage.rs::Row::embedding` から
/// 別途参照する）。構造検証は [`scan_scalar_columns`] に委譲し、本関数はその借用
/// 結果を `try_reserve_exact` + `push_str` で `Value::Text` へ複製するだけの薄い
/// ラッパー（Issue #56 レビュー指摘対応・codex P1: 全列を無条件に確保する経路は
/// テスト・小規模スキーマ向けの汎用 API としてのみ残し、可視行を多数走査する
/// `sql::exec.rs` の本番経路は [`scan_scalar_columns`] を直接使い不要な列を
/// 確保しない設計へ切り替えた）。`Text` 列は [`decode_row`] と同じ規則（バッファ
/// 末尾で打ち切られた nullable 列は `Value::Null`、non-nullable は `Err`。presence
/// タグ不正・宣言長が残りバッファを超える場合は `Err`）でデコードする。
pub fn decode_scalar_columns(schema: &TableSchema, buf: &[u8]) -> Result<Vec<Value>> {
    let scanned = scan_scalar_columns(schema, buf)?;
    let mut values: Vec<Value> = Vec::new();
    values.try_reserve_exact(scanned.len()).map_err(|_| {
        RowCodecError::Invalid("failed to reserve decoded scalar columns".to_string())
    })?;
    for slot in scanned {
        match slot {
            None => values.push(Value::Null),
            Some(ScalarRef::Text(text)) => {
                let mut owned = String::new();
                owned.try_reserve_exact(text.len()).map_err(|_| {
                    RowCodecError::Invalid("failed to reserve text field".to_string())
                })?;
                owned.push_str(text);
                values.push(Value::Text(owned));
            }
            Some(ScalarRef::Integer(v)) => values.push(Value::Integer(v)),
            Some(ScalarRef::BigInt(v)) => values.push(Value::BigInt(v)),
            Some(ScalarRef::Real(v)) => values.push(Value::Real(v)),
            Some(ScalarRef::Double(v)) => values.push(Value::Double(v)),
            Some(ScalarRef::Enum(label)) => {
                let mut owned = String::new();
                owned.try_reserve_exact(label.len()).map_err(|_| {
                    RowCodecError::Invalid("failed to reserve enum field".to_string())
                })?;
                owned.push_str(label);
                values.push(Value::Enum(owned));
            }
            Some(ScalarRef::Bool(b)) => values.push(Value::Bool(b)),
            Some(ScalarRef::Date(days)) => values.push(Value::Date(days)),
            Some(ScalarRef::Timestamp(micros)) => values.push(Value::Timestamp(micros)),
            Some(ScalarRef::Array(array_ref)) => {
                values.push(Value::Array(array_ref.to_value()?));
            }
            Some(ScalarRef::Bytes(bytes)) => {
                let mut owned: Vec<u8> = Vec::new();
                owned.try_reserve_exact(bytes.len()).map_err(|_| {
                    RowCodecError::Invalid("failed to reserve bytea field".to_string())
                })?;
                owned.extend_from_slice(bytes);
                values.push(Value::Bytes(owned));
            }
            Some(ScalarRef::Json(text)) => {
                let mut owned = String::new();
                owned.try_reserve_exact(text.len()).map_err(|_| {
                    RowCodecError::Invalid("failed to reserve json field".to_string())
                })?;
                owned.push_str(text);
                values.push(Value::Json(owned));
            }
            Some(ScalarRef::Numeric(d)) => values.push(Value::Numeric(d)),
            Some(ScalarRef::Uuid(u)) => values.push(Value::Uuid(u)),
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn text_vector_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        )
    }

    /// `REAL`／`DOUBLE PRECISION` 列（TABLE-13・TASK-196）の往復テスト用スキーマ。
    fn float_schema() -> TableSchema {
        TableSchema::new(
            "metrics",
            vec![
                ColumnDef::new("score", ColumnType::Real, false),
                ColumnDef::new("weight", ColumnType::Double, true),
            ],
        )
    }

    #[test]
    fn encode_decode_roundtrip_preserves_row() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello world".to_string()),
            Value::Null,
        ];
        let encoded =
            encode_row(&schema, "tenant-a", Visibility::Private, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.tenant_id, "tenant-a");
        assert_eq!(decoded.visibility, Visibility::Private);
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn encode_decode_roundtrip_preserves_real_and_double_boundary_values() {
        let schema = float_schema();
        for (real, double) in [
            (0.0f32, 0.0f64),
            (-0.0f32, -0.0f64), // 正規化されて +0.0 になる（F4）
            (f32::MAX, f64::MAX),
            (f32::MIN, f64::MIN),
            (f32::MIN_POSITIVE, f64::MIN_POSITIVE),
            (0.1f32, 0.1f64),
        ] {
            let values = vec![Value::Real(real), Value::Double(double)];
            let encoded =
                encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
            let decoded = decode_row(&schema, &encoded).expect("decode");
            match &decoded.values[0] {
                Value::Real(v) => assert_eq!(
                    v.to_bits(),
                    crate::scalar_float::canonicalize_real(real).to_bits()
                ),
                other => panic!("expected Real, got {other:?}"),
            }
            match &decoded.values[1] {
                Value::Double(v) => assert_eq!(
                    v.to_bits(),
                    crate::scalar_float::canonicalize_double(double).to_bits()
                ),
                other => panic!("expected Double, got {other:?}"),
            }
        }
    }

    #[test]
    fn encode_row_normalizes_signed_zero_to_positive_zero_in_stored_bytes() {
        // F4: `-0.0` はエンコード時点で `+0.0` へ正規化される契約。decode 側の
        // 正規化（encode_decode_roundtrip_preserves_real_and_double_boundary_values）
        // だけに頼ると、保存バイト列そのものは符号付きゼロのまま残り、
        // `content_hash`（台帳の 23505／22023 判定）の入力が読み出し結果と
        // 食い違う。ここでは encode_row が返す生バイト列を直接比較し、
        // `-0.0`／`+0.0` が同一バイト列に写ることを固定する。
        let schema = float_schema();
        let values_pos = vec![Value::Real(0.0), Value::Double(0.0)];
        let values_neg = vec![Value::Real(-0.0), Value::Double(-0.0)];
        let encoded_pos =
            encode_row(&schema, "tenant-a", Visibility::Public, &values_pos).expect("encode");
        let encoded_neg =
            encode_row(&schema, "tenant-a", Visibility::Public, &values_neg).expect("encode");
        assert_eq!(encoded_pos, encoded_neg);
    }

    #[test]
    fn encode_rejects_non_finite_real_and_double() {
        let schema = float_schema();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let values = vec![Value::Real(bad), Value::Null];
            assert!(matches!(
                encode_row(&schema, "tenant-a", Visibility::Public, &values),
                Err(RowCodecError::Invalid(_))
            ));
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let values = vec![Value::Real(1.0), Value::Double(bad)];
            assert!(matches!(
                encode_row(&schema, "tenant-a", Visibility::Public, &values),
                Err(RowCodecError::Invalid(_))
            ));
        }
    }

    #[test]
    fn encode_rejects_type_mismatch_for_real_and_double_columns() {
        let schema = float_schema();
        // score 列（Real）へ Double 値、weight 列（Double）へ Real 値を渡すのは
        // 型不一致として拒否する。
        let values = vec![Value::Double(1.0), Value::Real(1.0)];
        assert!(matches!(
            encode_row(&schema, "tenant-a", Visibility::Public, &values),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_non_finite_persisted_real_and_double_bytes() {
        // 永続バイトも untrusted とみなし、NaN/Inf のビット列は decode 時点で
        // 拒否する（TABLE-7 と同じ方針）。
        let schema = float_schema();
        let values = vec![Value::Real(1.0), Value::Double(1.0)];
        let mut encoded =
            encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        // ヘッダ（version(1)+visibility(1)+tenant_len(1)+"tenant-a"(8)）の直後、
        // presence(1) を挟んだ位置から始まる 4 バイトが REAL の LE ビット列。
        let real_bytes_offset = 1 + 1 + 1 + "tenant-a".len() + 1;
        encoded[real_bytes_offset..real_bytes_offset + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(matches!(
            decode_row(&schema, &encoded),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_real_and_double_fields() {
        let schema = float_schema();
        // 2 列目（weight, DOUBLE, nullable）も値ありにすることで、末尾切り詰めが
        // 「nullable 列の欠落」（許容される）ではなく「DOUBLE 本体の途中で
        // 打ち切られている」（拒否される）ことを保証する。
        let values = vec![Value::Real(1.0), Value::Double(2.0)];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        // DOUBLE は 8 バイト固定長のため、末尾を切り詰めると構造検証で拒否される。
        let truncated = &encoded[..encoded.len() - 1];
        assert!(matches!(
            decode_row(&schema, truncated),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn encode_rejects_short_field_length_overflow_without_truncating() {
        // MAX_TENANT_ID_LEN(=255) を超える tenant_id は Err になり、
        // 剰余に切り詰めた値で成功してはならない（TABLE-7 の核心）。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("body".to_string()),
            Value::Null,
        ];
        let huge_tenant = "t".repeat(256);
        let result = encode_row(&schema, &huge_tenant, Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_text_field_length_overflow() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let huge_text = "x".repeat((MAX_TEXT_FIELD_LEN as usize) + 1);
        let values = vec![Value::Text(huge_text)];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    /// 列ごとには [`MAX_TEXT_FIELD_LEN`] 以下でも、複数 `TEXT` 列の合計が
    /// [`MAX_SCALAR_PAYLOAD_LEN`] を超える場合は `encode_scalar_columns` が
    /// 確保前に拒否する（codex-review 指摘・PR #989「小さい SET でも既存の
    /// 大きな行に対し無制限確保が可能」対応。列数を増やして列単体の上限検査
    /// だけでは検出できない累計超過を再現する）。
    #[test]
    fn encode_scalar_columns_rejects_cumulative_text_length_overflow() {
        // 各列は MAX_TEXT_FIELD_LEN の半分強に収め、2 列の合計が
        // MAX_SCALAR_PAYLOAD_LEN（= MAX_TEXT_FIELD_LEN と同値）を超えるようにする。
        let per_column_len = (MAX_TEXT_FIELD_LEN as usize) / 2 + 1;
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", ColumnType::Text, false),
                ColumnDef::new("b", ColumnType::Text, false),
            ],
        );
        let values = vec![
            Value::Text("x".repeat(per_column_len)),
            Value::Text("y".repeat(per_column_len)),
        ];
        let result = encode_scalar_columns(&schema, &values);
        assert!(
            matches!(result, Err(RowCodecError::Invalid(_))),
            "cumulative scalar payload length must be rejected before allocation"
        );
    }

    /// `JSON` 列 1 個だけが対象行のスカラーペイロードを占める場合、
    /// `json::MAX_JSON_FIELD_LEN`（フレーミング込みでちょうど
    /// `MAX_SCALAR_PAYLOAD_LEN` に収まるよう定義済み）ちょうどの本文が
    /// `encode_scalar_columns` を成功させることを固定する（PR #1014 レビュー
    /// 指摘対応。旧定義〔`MAX_TEXT_FIELD_LEN` と同値〕では、束縛層
    /// （`sql::parser::bind_json_literal`）が受理したこの境界値の入力が、
    /// presence(1) + 長さ(4) バイトのフレーミング分だけ `MAX_SCALAR_PAYLOAD_LEN`
    /// を超過し、`encode_scalar_columns` 側で拒否され得た）。
    #[test]
    fn encode_scalar_columns_accepts_json_column_at_exact_field_length_limit() {
        let target = crate::json::MAX_JSON_FIELD_LEN;
        // 単一の文字列リテラルは `MAX_JSON_STRING_CHARS`（1 MiB）に抵触するため、
        // `json.rs` の境界値テストと同じ方式（複数要素の配列）で目標バイト数を
        // ちょうど組み立てる: `[` + 5 要素（`"`×2 + 本体） + 4 個の `,` + `]`。
        let overhead = 1 + 1 + 4 + 5 * 2;
        let content_total = target - overhead;
        let base = content_total / 5;
        let remainder = content_total % 5;
        let lens = [base, base, base, base, base + remainder];
        assert!(lens.iter().all(|&l| l < crate::json::MAX_JSON_STRING_CHARS));
        let mut doc = String::new();
        doc.push('[');
        for (i, len) in lens.iter().enumerate() {
            if i > 0 {
                doc.push(',');
            }
            doc.push('"');
            doc.push_str(&"a".repeat(*len));
            doc.push('"');
        }
        doc.push(']');
        assert_eq!(doc.len(), target);
        crate::json::validate_json_column_text(&doc).expect("valid json at exact limit");

        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("doc", ColumnType::Json, true),
            ],
        );
        let values = vec![Value::Vector(vec![0.1, 0.2]), Value::Json(doc)];
        let encoded = encode_scalar_columns(&schema, &values)
            .expect("a single MAX_JSON_FIELD_LEN column must fit MAX_SCALAR_PAYLOAD_LEN");
        assert_eq!(encoded.len() as u32, MAX_SCALAR_PAYLOAD_LEN);
    }

    #[test]
    fn encode_decode_rejects_vector_dim_mismatch() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0]), // 宣言次元 3 に対し 2
            Value::Text("body".to_string()),
            Value::Null,
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_decode_rejects_vector_dim_exceeding_max() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "embedding",
                ColumnType::Vector(MAX_EMBEDDING_DIM + 1),
                false,
            )],
        );
        // catalog 層の validate_vector_dim は MAX_VECTOR_DIM 超過を schema 作成時点で
        // 拒否するが、本テストは row_codec 単体での上限検証（多層防御）を確認する。
        let huge_vector = vec![0.0f32; (MAX_EMBEDDING_DIM as usize) + 1];
        let values = vec![Value::Vector(huge_vector)];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_null_for_non_nullable_column() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Null, // body は non-nullable
            Value::Null,
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_visibility_byte() {
        let schema = text_vector_schema();
        let mut buf = vec![ROW_CODEC_FORMAT_VERSION, 0xff, 1, b't'];
        for column in &schema.columns {
            buf.push(PRESENCE_NULL);
            let _ = column;
        }
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_presence_byte() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        let mut buf = vec![
            ROW_CODEC_FORMAT_VERSION,
            Visibility::Public.to_byte(),
            1,
            b't',
        ];
        buf.push(0xaa); // 未知の presence バイト
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_unknown_format_version() {
        let schema = text_vector_schema();
        let buf = vec![0xff];
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_truncated_buffer_at_each_field_boundary() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Null,
        ];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        for cut in 1..encoded.len() {
            let truncated = &encoded[..cut];
            let result = decode_row(&schema, truncated);
            // 末尾の nullable 列欠落は許容されるため、途中切断のうち少なくとも
            // ヘッダ・embedding・body 境界の切断はすべて Err になることを確認する。
            if cut < encoded.len() - 1 {
                assert!(
                    result.is_err(),
                    "expected Err when truncated at byte {cut}, got {result:?}"
                );
            }
        }
    }

    #[test]
    fn decode_treats_missing_trailing_nullable_column_as_null() {
        // TABLE-5 前提: ADD COLUMN で追加された nullable 列を持たない既存行を
        // デコードすると、その列は Null として読める。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
        ];
        // tag 列（nullable, 末尾）を含めずにエンコードする（欠落バイト列を模す）。
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&3u32.to_le_bytes());
        for v in [1.0f32, 2.0, 3.0] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(b"hello");
        // tag 列を書かない（バッファをここで終える）。

        let decoded = decode_row(&schema, &buf).expect("decode should succeed");
        assert_eq!(decoded.values.len(), 3);
        assert_eq!(decoded.values[2], Value::Null);
        let _ = values;
    }

    #[test]
    fn decode_rejects_missing_trailing_non_nullable_column() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        // body 列（non-nullable）を書かずにバッファを終える。
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_rejects_more_values_than_schema_columns() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let values = vec![
            Value::Text("a".to_string()),
            Value::Text("b".to_string()), // スキーマの列数を超える
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_rejects_declared_length_exceeding_remaining_buffer() {
        // 宣言長が残りバッファ長を超える不正入力は、アロケーション前に Err になる
        // （text_len を故意に巨大な値にする）。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&u32::MAX.to_le_bytes()); // 宣言長が残りバッファを大幅に超過
        buf.extend_from_slice(b"short");
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn non_empty_body_roundtrips() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let body = "この行の body 列は単一保管され、別ストアへ複製・圧縮しない（TASK-86 判断）。"
            .repeat(10);
        let values = vec![Value::Text(body.clone())];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values[0], Value::Text(body));
    }

    // --- encode_scalar_columns / decode_scalar_columns（TASK-75、SQL-2） -----------

    #[test]
    fn scalar_columns_roundtrip_skips_vector_column() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello world".to_string()),
            Value::Text("tag-a".to_string()),
        ];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], Value::Null); // VECTOR 列はダミー Null
        assert_eq!(decoded[1], Value::Text("hello world".to_string()));
        assert_eq!(decoded[2], Value::Text("tag-a".to_string()));
    }

    #[test]
    fn scalar_columns_treats_missing_trailing_nullable_column_as_null() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            // tag（nullable, 末尾）を渡さない。
        ];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded[2], Value::Null);
    }

    #[test]
    fn scalar_columns_rejects_missing_trailing_non_nullable_column() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        );
        let values = vec![Value::Vector(vec![1.0, 2.0])];
        let result = encode_scalar_columns(&schema, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_decode_rejects_unknown_presence_byte() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        let buf = vec![0xaa]; // 未知の presence バイト
        let result = decode_scalar_columns(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_decode_rejects_declared_length_exceeding_remaining_buffer() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(b"short");
        let result = decode_scalar_columns(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scalar_columns_encode_rejects_text_field_length_overflow() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let huge_text = "x".repeat((MAX_TEXT_FIELD_LEN as usize) + 1);
        let values = vec![Value::Text(huge_text)];
        let result = encode_scalar_columns(&schema, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    // --- scan_scalar_columns（Issue #56 レビュー指摘対応・codex P1） ----------------

    #[test]
    fn scan_scalar_columns_borrows_text_without_copying() {
        // `Text` 値のポインタが `buf` の範囲内に収まることを確認し、`to_string()` の
        // ような複製が発生していないことを直接検証する（不要列を確保前にスキップ
        // する設計の核心。呼び出し元 `sql::exec.rs::on_visible_row` はこの borrow を
        // 使って投影に不要な列を確保せずに読み飛ばす）。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("body-text".repeat(1000)),
            Value::Text("tag-value".to_string()),
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan scalar");
        assert_eq!(scanned.len(), 3);
        assert_eq!(scanned[0], None); // VECTOR 列は常に None
        let buf_range = buf.as_ptr() as usize..buf.as_ptr() as usize + buf.len();
        for slot in scanned.iter().skip(1) {
            let s = slot
                .and_then(|v| v.as_text())
                .expect("text column must be Some");
            let ptr = s.as_ptr() as usize;
            assert!(
                buf_range.contains(&ptr),
                "scanned &str must borrow from buf, not allocate a copy"
            );
        }
        assert_eq!(scanned[1], Some(ScalarRef::Text(&"body-text".repeat(1000))));
        assert_eq!(scanned[2], Some(ScalarRef::Text("tag-value")));
    }

    #[test]
    fn scan_scalar_columns_rejects_declared_length_exceeding_max_before_reading_bytes() {
        // 宣言長が MAX_TEXT_FIELD_LEN を超える場合、残りバッファの実サイズによらず
        // 読み出し(スライス化)より前に Err になる(borrow 版でも上限検証がバイト
        // アクセスに先立つことを確認する)。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&(MAX_TEXT_FIELD_LEN + 1).to_le_bytes());
        buf.extend_from_slice(b"short");
        let result = scan_scalar_columns(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scan_scalar_columns_skips_vector_columns_and_matches_decode_scalar_columns() {
        // `decode_scalar_columns`（複製あり）は `scan_scalar_columns`（借用のみ）の
        // 薄いラッパーへ再実装されたため、両者の観測結果が一致することを確認する。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Null,
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan scalar");
        let decoded = decode_scalar_columns(&schema, &buf).expect("decode scalar");
        assert_eq!(decoded.len(), scanned.len());
        for (slot, value) in scanned.iter().zip(decoded.iter()) {
            match (slot, value) {
                (None, Value::Null) => {}
                (Some(ScalarRef::Text(s)), Value::Text(t)) => assert_eq!(*s, t.as_str()),
                other => panic!("scan/decode mismatch: {other:?}"),
            }
        }
    }

    // --- scan_scalar_columns_masked（Issue #350: 必要列限定デコード） ------------

    #[test]
    fn scan_scalar_columns_masked_none_mask_matches_scan_scalar_columns() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Text("world".to_string()),
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let via_none = scan_scalar_columns_masked(&schema, &buf, None).expect("scan masked none");
        let via_scan = scan_scalar_columns(&schema, &buf).expect("scan scalar");
        assert_eq!(via_none, via_scan);
    }

    #[test]
    fn scan_scalar_columns_masked_unwanted_column_is_none_but_wanted_column_still_decodes() {
        // `body`（マスク対象外）は presence・長さ・境界の構造検証だけを通過して
        // `None` になり、`tag`（マスク対象）は通常どおり `&str` 化される。
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("unwanted-but-structurally-valid".to_string()),
            Value::Text("wanted".to_string()),
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        // mask: [embedding(無視), body(不要), tag(必要)]
        let mask = [false, false, true];
        let scanned = scan_scalar_columns_masked(&schema, &buf, Some(&mask)).expect("scan masked");
        assert_eq!(scanned[0], None); // VECTOR 列は常に None
        assert_eq!(scanned[1], None); // マスク対象外
        assert_eq!(scanned[2], Some(ScalarRef::Text("wanted")));
    }

    #[test]
    fn scan_scalar_columns_masked_unwanted_column_with_invalid_utf8_is_rejected() {
        // マスク対象外の列でも UTF-8 妥当性検証は省略しない（codex-review P1
        // 指摘・PR #369: 不正な UTF-8 バイト列を含む永続行は、参照されない列
        // 経由であっても従来どおり `Err`（`XX000`）で拒否する。省略するのは
        // 検証済み値の `&str` 生成・保持のみ）。
        let schema = text_vector_schema();
        let mut buf = Vec::new();
        // VECTOR 列（embedding）は presence バイト自体を持たないため、
        // 先頭は body（マスク対象外）の presence から書き始める。
        let invalid_utf8: &[u8] = &[0xff, 0xfe, 0xfd];
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&(invalid_utf8.len() as u32).to_le_bytes());
        buf.extend_from_slice(invalid_utf8);
        // tag（nullable）は省略（バッファ末尾で打ち切り = NULL として許容）。
        let mask = [false, false, false];
        let result = scan_scalar_columns_masked(&schema, &buf, Some(&mask));
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn scan_scalar_columns_masked_mask_length_mismatch_is_rejected() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Null,
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let short_mask = [true, true];
        let result = scan_scalar_columns_masked(&schema, &buf, Some(&short_mask));
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    // --- validate_scalar_columns（Issue #350・PR #369 codex-review P1 対応:
    // `DecodeTier::Fast` 用の検証専用・Vec 確保なし API） --------------------------

    #[test]
    fn validate_scalar_columns_accepts_structurally_valid_row() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Text("world".to_string()),
        ];
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        assert!(validate_scalar_columns(&schema, &buf).is_ok());
    }

    #[test]
    fn validate_scalar_columns_rejects_invalid_utf8_same_as_masked_scan() {
        // `scan_scalar_columns_masked` の全 `false` マスクと同じ破損検知を、
        // 検証専用 API でも維持することを確認する（走査本体
        // `scan_scalar_columns_validated` の共有を裏付ける）。
        let schema = text_vector_schema();
        let mut buf = Vec::new();
        let invalid_utf8: &[u8] = &[0xff, 0xfe, 0xfd];
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&(invalid_utf8.len() as u32).to_le_bytes());
        buf.extend_from_slice(invalid_utf8);
        assert!(matches!(
            validate_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
        let mask = [false, false, false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn validate_scalar_columns_rejects_trailing_bytes() {
        let schema = text_vector_schema();
        let values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Text("world".to_string()),
        ];
        let mut buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        buf.push(0xff);
        assert!(matches!(
            validate_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
    }

    // --- INTEGER / BIGINT（Issue #881・TABLE-13・TASK-196） -----------------

    fn integer_bigint_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("nn", ColumnType::Integer, true),
            ],
        )
    }

    #[test]
    fn encode_decode_roundtrip_preserves_integer_and_bigint_boundary_values() {
        let schema = integer_bigint_schema();
        let values = vec![
            Value::Integer(i32::MIN),
            Value::BigInt(i64::MAX),
            Value::Null,
        ];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values, values);

        let values2 = vec![
            Value::Integer(i32::MAX),
            Value::BigInt(i64::MIN),
            Value::Integer(0),
        ];
        let encoded2 =
            encode_row(&schema, "tenant-a", Visibility::Public, &values2).expect("encode");
        let decoded2 = decode_row(&schema, &encoded2).expect("decode");
        assert_eq!(decoded2.values, values2);
    }

    #[test]
    fn encode_rejects_integer_value_for_bigint_column_and_vice_versa() {
        let schema = integer_bigint_schema();
        let values = vec![Value::BigInt(1), Value::BigInt(2), Value::Null];
        assert!(matches!(
            encode_row(&schema, "tenant-a", Visibility::Public, &values),
            Err(RowCodecError::Invalid(_))
        ));
        let values2 = vec![Value::Integer(1), Value::Integer(2), Value::Null];
        assert!(matches!(
            encode_row(&schema, "tenant-a", Visibility::Public, &values2),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_buffer_at_integer_and_bigint_field_boundaries() {
        let schema = integer_bigint_schema();
        let values = vec![Value::Integer(42), Value::BigInt(-7), Value::Null];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        // 末尾（nullable 列の NULL 1 バイト）を除く全ての切断点で Err になる。
        for cut in 1..encoded.len() - 1 {
            let truncated = &encoded[..cut];
            assert!(
                decode_row(&schema, truncated).is_err(),
                "expected Err when truncated at byte {cut}"
            );
        }
    }

    #[test]
    fn scalar_payload_roundtrip_preserves_integer_and_bigint_and_leaves_text_only_bytes_unchanged()
    {
        let schema = integer_bigint_schema();
        let values = vec![Value::Integer(7), Value::BigInt(-9), Value::Integer(0)];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded, values);

        // TEXT/VECTOR のみの既存スキーマでは、スカラーペイロードのバイト列が
        // 本 Issue の前後で完全に不変であることを固定する（golden）。
        let text_schema = text_vector_schema();
        let text_values = vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Null,
        ];
        let text_encoded = encode_scalar_columns(&text_schema, &text_values).expect("encode");
        // presence(1) + len(4) + "hello"(5) = 10、続いて NULL の presence(1)。
        assert_eq!(
            text_encoded,
            vec![
                PRESENCE_VALUE,
                5,
                0,
                0,
                0,
                b'h',
                b'e',
                b'l',
                b'l',
                b'o',
                PRESENCE_NULL,
            ]
        );
    }

    #[test]
    fn merge_encode_scalar_columns_rewrites_existing_integer_and_bigint_values() {
        let schema = integer_bigint_schema();
        let existing = [
            Some(ScalarRef::Integer(1)),
            Some(ScalarRef::BigInt(2)),
            None,
        ];
        // SET 対象は列 1（BigInt）のみ。列 0・2 は既存値をそのまま書き戻す。
        let new_bigint = Value::BigInt(999);
        let overrides: Vec<(usize, &Value)> = vec![(1, &new_bigint)];
        let merged =
            merge_encode_scalar_columns(&schema, &existing, &overrides).expect("merge encode");
        let decoded = decode_scalar_columns(&schema, &merged).expect("decode merged");
        assert_eq!(
            decoded,
            vec![Value::Integer(1), Value::BigInt(999), Value::Null]
        );
    }

    #[test]
    fn scan_scalar_columns_returns_integer_and_bigint_as_scalar_ref() {
        let schema = integer_bigint_schema();
        let values = vec![Value::Integer(5), Value::BigInt(-5), Value::Null];
        let buf = encode_scalar_columns(&schema, &values).expect("encode");
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(
            scanned,
            vec![
                Some(ScalarRef::Integer(5)),
                Some(ScalarRef::BigInt(-5)),
                None,
            ]
        );
    }

    fn array_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 8).expect("array ty")),
                    false,
                ),
                ColumnDef::new(
                    "flags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).expect("array ty")),
                    true,
                ),
            ],
        )
    }

    #[test]
    fn array_encode_decode_roundtrip_text_and_bool() {
        let schema = array_schema();
        let values = vec![
            Value::Array(ArrayValue::Text(vec![
                Some("a".to_string()),
                Some("b b".to_string()),
                Some(String::new()),
            ])),
            Value::Array(ArrayValue::Bool(vec![Some(true), Some(false), Some(true)])),
        ];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn array_encode_decode_roundtrip_empty_array() {
        let schema = array_schema();
        let values = vec![
            Value::Array(ArrayValue::Text(vec![])),
            Value::Array(ArrayValue::Bool(vec![])),
        ];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn array_null_column_and_empty_array_are_distinct() {
        let schema = array_schema();
        let values = vec![Value::Array(ArrayValue::Text(vec![])), Value::Null];
        let encoded = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &encoded).expect("decode");
        assert_eq!(decoded.values[1], Value::Null);
        assert_eq!(decoded.values[0], Value::Array(ArrayValue::Text(vec![])));
    }

    #[test]
    fn array_encode_rejects_element_count_exceeding_max_len() {
        let schema = array_schema();
        let too_many = vec![Some("x".to_string()); 9]; // max_len = 8
        let values = vec![Value::Array(ArrayValue::Text(too_many)), Value::Null];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn array_encode_rejects_element_type_mismatch() {
        let schema = array_schema();
        let values = vec![
            Value::Array(ArrayValue::Bool(vec![Some(true)])), // tags is Text
            Value::Null,
        ];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn array_decode_rejects_flags_byte_nonzero() {
        let schema = array_schema();
        let values = vec![Value::Array(ArrayValue::Text(vec![])), Value::Null];
        let mut encoded =
            encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        // ヘッダ(3) + tenant_id("tenant-a"=8) + presence(1) の直後が flags バイト。
        let flags_offset = 3 + "tenant-a".len() + 1;
        encoded[flags_offset] = 0x01;
        assert!(matches!(
            decode_row(&schema, &encoded),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn array_decode_rejects_invalid_utf8_text_element() {
        let schema = array_schema();
        // 1 要素・長さ 1 の TEXT 要素バイトを不正 UTF-8 に差し替える。
        let values = vec![
            Value::Array(ArrayValue::Text(vec![Some("a".to_string())])),
            Value::Null,
        ];
        let mut encoded =
            encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let elem_byte_offset = encoded.len() - 1;
        encoded[elem_byte_offset] = 0xff;
        assert!(matches!(
            decode_row(&schema, &encoded),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn array_decode_rejects_unknown_bool_element_byte() {
        let schema = array_schema();
        let values = vec![
            Value::Array(ArrayValue::Text(vec![])),
            Value::Array(ArrayValue::Bool(vec![Some(true)])),
        ];
        let mut encoded =
            encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let elem_byte_offset = encoded.len() - 1;
        encoded[elem_byte_offset] = 0x02;
        assert!(matches!(
            decode_row(&schema, &encoded),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn array_scalar_columns_scan_and_merge_roundtrip() {
        let schema = array_schema();
        let values = vec![
            Value::Array(ArrayValue::Text(vec![
                Some("x".to_string()),
                Some("y".to_string()),
            ])),
            Value::Array(ArrayValue::Bool(vec![Some(true)])),
        ];
        let encoded = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let scanned = scan_scalar_columns(&schema, &encoded).expect("scan");
        assert_eq!(scanned.len(), 2);
        match scanned[0] {
            Some(ScalarRef::Array(array_ref)) => {
                assert_eq!(array_ref.elem(), ArrayElemType::Text);
                assert_eq!(array_ref.count(), 2);
                assert_eq!(
                    array_ref.to_value().expect("to_value"),
                    ArrayValue::Text(vec![Some("x".to_string()), Some("y".to_string())])
                );
            }
            _ => panic!("expected ScalarRef::Array for tags column"),
        }

        // merge_encode_scalar_columns: SET 対象でない列（既存の Array 借用値）を
        // そのまま書き込めること。
        let overrides: Vec<(usize, &Value)> = vec![];
        let merged =
            merge_encode_scalar_columns(&schema, &scanned, &overrides).expect("merge encode");
        assert_eq!(merged, encoded);

        let decoded = decode_scalar_columns(&schema, &encoded).expect("decode scalar");
        assert_eq!(decoded, values);
    }

    #[test]
    fn array_type_rejects_max_len_out_of_range() {
        assert!(ArrayType::new(ArrayElemType::Text, 0).is_err());
        assert!(ArrayType::new(ArrayElemType::Text, MAX_ARRAY_ELEMENTS + 1).is_err());
        assert!(ArrayType::new(ArrayElemType::Text, MAX_ARRAY_ELEMENTS).is_ok());
    }

    // --- BYTEA 列（Issue #886）の decode-side DoS ガード ------------------------

    fn bytea_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![ColumnDef::new("blob", ColumnType::Bytea, true)],
        )
    }

    #[test]
    fn decode_row_rejects_bytea_declared_length_over_limit_before_body_read() {
        let schema = bytea_schema();
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        // 宣言長は上限超過（MAX + 1）。本体は短いままにする——長さ検査が
        // 本体バッファの実サイズによらず先に効くことを固定する（B3）。
        buf.extend_from_slice(&(crate::bytea::MAX_BYTEA_FIELD_LEN + 1).to_le_bytes());
        buf.extend_from_slice(b"short");
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn decode_row_rejects_truncated_bytea_body() {
        let schema = bytea_schema();
        let mut buf = Vec::new();
        buf.push(ROW_CODEC_FORMAT_VERSION);
        buf.push(Visibility::Public.to_byte());
        let tenant_bytes = b"tenant-a";
        buf.push(tenant_bytes.len() as u8);
        buf.extend_from_slice(tenant_bytes);
        buf.push(PRESENCE_VALUE);
        // 宣言長は 10 バイトだが本体は 3 バイトしかない。
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(b"abc");
        let result = decode_row(&schema, &buf);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_row_rejects_bytes_value_for_text_column() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let values = vec![Value::Bytes(vec![0xde, 0xad])];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn encode_row_rejects_text_value_for_bytea_column() {
        let schema = bytea_schema();
        let values = vec![Value::Text("hello".to_string())];
        let result = encode_row(&schema, "tenant-a", Visibility::Public, &values);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn bytea_round_trips_through_encode_decode_row() {
        let schema = bytea_schema();
        let values = vec![Value::Bytes(vec![0x00, 0xff, 0xde, 0xad])];
        let buf = encode_row(&schema, "tenant-a", Visibility::Public, &values).expect("encode");
        let decoded = decode_row(&schema, &buf).expect("decode");
        assert_eq!(decoded.values, values);
    }

    #[test]
    fn bytea_null_and_empty_are_distinct_after_round_trip() {
        let schema = bytea_schema();
        let null_buf = encode_row(&schema, "tenant-a", Visibility::Public, &[Value::Null])
            .expect("encode null");
        let empty_buf = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[Value::Bytes(Vec::new())],
        )
        .expect("encode empty");
        assert_ne!(null_buf, empty_buf);
        assert_eq!(
            decode_row(&schema, &null_buf).expect("decode null").values,
            vec![Value::Null]
        );
        assert_eq!(
            decode_row(&schema, &empty_buf)
                .expect("decode empty")
                .values,
            vec![Value::Bytes(Vec::new())]
        );
    }

    // --- DATE/TIMESTAMP encode 時の範囲検証（codex-review／Cursor Bugbot 指摘。
    // decode 側（decode_row・scan_scalar_columns）は範囲外値を拒否するのに対し
    // encode 側が検証していないと、公開 enum Value::Date/Timestamp を直接
    // 構築する呼び出し元（SQL パーサーを経由しない Rust API 経路を含む）から
    // 範囲外値を書き込み成功させてしまい、書き込んだ本人がその行を二度と
    // 読めなくなる非対称な永続化バグになる。encode_row・encode_scalar_columns・
    // merge_encode_scalar_columns の 3 箇所すべてで拒否されることを固定する） ---

    fn date_timestamp_schema() -> TableSchema {
        TableSchema::new(
            "events",
            vec![
                ColumnDef::new("d", ColumnType::Date, true),
                ColumnDef::new("t", ColumnType::Timestamp, true),
            ],
        )
    }

    #[test]
    fn encode_row_rejects_out_of_range_date_and_timestamp() {
        let schema = date_timestamp_schema();
        let result = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[Value::Date(crate::datetime::DATE_MAX_DAYS + 1), Value::Null],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        let result = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[
                Value::Null,
                Value::Timestamp(crate::datetime::TIMESTAMP_MAX_MICROS + 1),
            ],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        let result = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[Value::Date(crate::datetime::DATE_MIN_DAYS - 1), Value::Null],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        let result = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[
                Value::Null,
                Value::Timestamp(crate::datetime::TIMESTAMP_MIN_MICROS - 1),
            ],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        // 範囲内の値は従来どおり成功し decode で読み戻せる（非対称でないことの確認）。
        let ok = encode_row(
            &schema,
            "tenant-a",
            Visibility::Public,
            &[
                Value::Date(crate::datetime::DATE_MAX_DAYS),
                Value::Timestamp(crate::datetime::TIMESTAMP_MIN_MICROS),
            ],
        )
        .expect("encode in-range date/timestamp");
        let decoded = decode_row(&schema, &ok).expect("decode in-range date/timestamp");
        assert_eq!(
            decoded.values,
            vec![
                Value::Date(crate::datetime::DATE_MAX_DAYS),
                Value::Timestamp(crate::datetime::TIMESTAMP_MIN_MICROS),
            ]
        );
    }

    #[test]
    fn encode_scalar_columns_rejects_out_of_range_date_and_timestamp() {
        let schema = date_timestamp_schema();
        let result = encode_scalar_columns(
            &schema,
            &[Value::Date(crate::datetime::DATE_MAX_DAYS + 1), Value::Null],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        let result = encode_scalar_columns(
            &schema,
            &[
                Value::Null,
                Value::Timestamp(crate::datetime::TIMESTAMP_MAX_MICROS + 1),
            ],
        );
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    #[test]
    fn merge_encode_scalar_columns_rejects_out_of_range_date_and_timestamp() {
        let schema = date_timestamp_schema();
        let out_of_range_date = Value::Date(crate::datetime::DATE_MAX_DAYS + 1);
        let result =
            merge_encode_scalar_columns(&schema, &[None, None], &[(0, &out_of_range_date)]);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));

        let out_of_range_ts = Value::Timestamp(crate::datetime::TIMESTAMP_MIN_MICROS - 1);
        let result = merge_encode_scalar_columns(&schema, &[None, None], &[(1, &out_of_range_ts)]);
        assert!(matches!(result, Err(RowCodecError::Invalid(_))));
    }

    // --- merge_encode_scalar_columns（Issue #996: 述語つき UPDATE の適用段を
    // 単一行 UPDATE と共有する経路へ統一する前提のバイト同一性検証） -------------

    /// `decode_scalar_columns`（全列複製）→ スロット上書き → `encode_scalar_columns`
    /// という旧・述語つき UPDATE 実装の経路を再現する参照オラクル。
    fn apply_overrides_via_decode_then_encode(
        schema: &TableSchema,
        buf: &[u8],
        overrides: &[(usize, &Value)],
    ) -> Vec<u8> {
        let mut merged_values = decode_scalar_columns(schema, buf).expect("decode scalar");
        for (idx, value) in overrides {
            let slot = merged_values
                .get_mut(*idx)
                .expect("override index must be in range");
            *slot = (*value).clone();
        }
        encode_scalar_columns(schema, &merged_values).expect("encode scalar")
    }

    /// `scan_scalar_columns`（借用のみ）→ `merge_encode_scalar_columns` という
    /// 単一行 UPDATE・統一後の述語つき UPDATE が共有する経路。
    fn apply_overrides_via_scan_then_merge(
        schema: &TableSchema,
        buf: &[u8],
        overrides: &[(usize, &Value)],
    ) -> Vec<u8> {
        let scanned = scan_scalar_columns(schema, buf).expect("scan scalar");
        merge_encode_scalar_columns(schema, &scanned, overrides).expect("merge encode scalar")
    }

    fn text_only_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        )
    }

    /// VECTOR 列が先頭でなく中間 index にあるスキーマ（`text_vector_schema` は
    /// embedding が index 0 に固定されているため、途中の index にある場合も
    /// 同じ経路を通ることを別途確認する）。
    fn text_vector_text_schema_vector_in_middle() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("tag", ColumnType::Text, true),
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    /// [`merge_encode_scalar_columns`]（借用版 `scan_scalar_columns` 経由）と
    /// [`decode_scalar_columns`]＋`encode_scalar_columns`（旧・述語つき UPDATE
    /// 実装が使っていた経路）が、同じ overrides に対して常にバイト同一の出力を
    /// 生成することを固定する（Issue #996）。`crates/engine/src/tenant.rs` の
    /// 述語つき UPDATE 適用段（`update_rows_where_unchecked`）を本関数へ統一する
    /// 前提となる等価性テスト。
    #[test]
    fn merge_encode_scalar_columns_matches_decode_then_encode_scalar_columns() {
        let schema = text_vector_schema();

        // nullable 列（tag）が NULL のまま（SET なし）。
        let buf = encode_scalar_columns(
            &schema,
            &[
                Value::Vector(vec![1.0, 2.0, 3.0]),
                Value::Text("body-text".to_string()),
                Value::Null,
            ],
        )
        .expect("encode scalar");
        assert_eq!(
            apply_overrides_via_scan_then_merge(&schema, &buf, &[]),
            apply_overrides_via_decode_then_encode(&schema, &buf, &[])
        );

        // 既存値 NULL → SET text（マルチバイト UTF-8 を含む）。
        let set_text = Value::Text("こんにちは".to_string());
        let overrides = [(2usize, &set_text)];
        assert_eq!(
            apply_overrides_via_scan_then_merge(&schema, &buf, &overrides),
            apply_overrides_via_decode_then_encode(&schema, &buf, &overrides)
        );

        // 既存値 text → SET NULL。
        let buf_with_tag = encode_scalar_columns(
            &schema,
            &[
                Value::Vector(vec![1.0, 2.0, 3.0]),
                Value::Text("body-text".to_string()),
                Value::Text("existing-tag".to_string()),
            ],
        )
        .expect("encode scalar");
        let set_null = Value::Null;
        let overrides = [(2usize, &set_null)];
        assert_eq!(
            apply_overrides_via_scan_then_merge(&schema, &buf_with_tag, &overrides),
            apply_overrides_via_decode_then_encode(&schema, &buf_with_tag, &overrides)
        );

        // 空文字列も含む複数 TEXT 列の一部だけを SET（non-nullable な body は
        // SET しない＝借用のまま連結される経路を通す）。
        let set_empty = Value::Text(String::new());
        let overrides = [(2usize, &set_empty)];
        assert_eq!(
            apply_overrides_via_scan_then_merge(&schema, &buf_with_tag, &overrides),
            apply_overrides_via_decode_then_encode(&schema, &buf_with_tag, &overrides)
        );

        // VECTOR 列を持たないスキーマ（`overrides` が空でも通ることを含む）。
        let text_only = text_only_schema();
        let buf_text_only =
            encode_scalar_columns(&text_only, &[Value::Text("body".to_string()), Value::Null])
                .expect("encode scalar");
        let set_tag = Value::Text("filled".to_string());
        let overrides = [(1usize, &set_tag)];
        assert_eq!(
            apply_overrides_via_scan_then_merge(&text_only, &buf_text_only, &overrides),
            apply_overrides_via_decode_then_encode(&text_only, &buf_text_only, &overrides)
        );
        assert_eq!(
            apply_overrides_via_scan_then_merge(&text_only, &buf_text_only, &[]),
            apply_overrides_via_decode_then_encode(&text_only, &buf_text_only, &[])
        );

        // VECTOR 列が途中の index にあるスキーマ。
        let mid_vector = text_vector_text_schema_vector_in_middle();
        let buf_mid_vector = encode_scalar_columns(
            &mid_vector,
            &[
                Value::Text("existing-tag".to_string()),
                Value::Vector(vec![4.0, 5.0]),
                Value::Text("body-text".to_string()),
            ],
        )
        .expect("encode scalar");
        let set_tag2 = Value::Null;
        let overrides = [(0usize, &set_tag2)];
        assert_eq!(
            apply_overrides_via_scan_then_merge(&mid_vector, &buf_mid_vector, &overrides),
            apply_overrides_via_decode_then_encode(&mid_vector, &buf_mid_vector, &overrides)
        );
    }

    // --- Issue #894: 新スカラー型（TABLE-13・TASK-199）のマスク付き走査網羅 -----
    //
    // 集計・広域取得の必要列限定デコード（Issue #350・`sql::aggregate::
    // ReferencedColumns`／`sql::scan::decode_tier_for`）は `scan_scalar_columns_
    // masked` を単一情報源とする。ここでは可変長・固定長列が交互に並ぶスキーマで
    // 単一ビットのマスクを総当たりし、`wanted` な列だけが値化されること、
    // 非要求列でも構造検証（presence・宣言長・値域）が一切省略されないことを
    // 固定する（受入条件 1）。`ENUM` は `Storage::create_enum_type` を要し本
    // ユニットテストの構成では作れないため対象外とし、`tests/enum_column.rs`
    // （既存）・`tests/decode_tier_scalar_types.rs`（Issue #894 新設）で検証する。

    /// 新スカラー型を可変長・固定長が交互になるよう並べたスキーマ。
    fn all_scalar_types_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true), // 0: VECTOR（常に None）
                ColumnDef::new("t", ColumnType::Text, false),             // 1: 可変長
                ColumnDef::new("i", ColumnType::Integer, false),          // 2: 固定長
                ColumnDef::new("by", ColumnType::Bytea, true),            // 3: 可変長
                ColumnDef::new("b", ColumnType::BigInt, false),           // 4: 固定長
                ColumnDef::new("j", ColumnType::Json, true),              // 5: 可変長
                ColumnDef::new("r", ColumnType::Real, false),             // 6: 固定長
                ColumnDef::new("u", ColumnType::Uuid, true),              // 7: 固定長
                ColumnDef::new("d", ColumnType::Double, false),           // 8: 固定長
                ColumnDef::new(
                    "a",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 8).expect("array ty")),
                    true,
                ), // 9: 可変長
                ColumnDef::new("bo", ColumnType::Boolean, false),         // 10: 固定長
                ColumnDef::new(
                    "n",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ), // 11: 固定長
                ColumnDef::new("dt", ColumnType::Date, true),             // 12: 固定長
                ColumnDef::new("ts", ColumnType::Timestamp, true),        // 13: 固定長
            ],
        )
    }

    /// 全列に非 NULL の値を持つ行（マスク総当たりの基準値）。
    fn all_scalar_types_values() -> Vec<Value> {
        vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Text("hello".to_string()),
            Value::Integer(42),
            Value::Bytes(vec![1, 2, 3]),
            Value::BigInt(123_456_789),
            Value::Json(r#"{"a":1}"#.to_string()),
            Value::Real(1.5),
            Value::Uuid(Uuid::from_bytes([9u8; 16])),
            Value::Double(2.5),
            Value::Array(ArrayValue::Text(vec![
                Some("x".to_string()),
                Some("y".to_string()),
            ])),
            Value::Bool(true),
            Value::Numeric(Decimal::from_parts(1234, 2).expect("decimal")),
            Value::Date(100),
            Value::Timestamp(123_456_789),
        ]
    }

    #[test]
    fn scan_scalar_columns_masked_all_scalar_types_single_bit_mask_sweep() {
        let schema = all_scalar_types_schema();
        let values = all_scalar_types_values();
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        let full_scan = scan_scalar_columns(&schema, &buf).expect("scan scalar");
        assert_eq!(full_scan.len(), schema.columns.len());
        assert_eq!(full_scan[0], None, "VECTOR column is always None");

        // 全 false マスク: すべて None。
        let all_false = vec![false; schema.columns.len()];
        let scanned_all_false =
            scan_scalar_columns_masked(&schema, &buf, Some(&all_false)).expect("scan all-false");
        assert!(scanned_all_false.iter().all(|slot| slot.is_none()));

        // 全 true マスク: `scan_scalar_columns`（マスクなし）と一致。
        let all_true = vec![true; schema.columns.len()];
        let scanned_all_true =
            scan_scalar_columns_masked(&schema, &buf, Some(&all_true)).expect("scan all-true");
        assert_eq!(scanned_all_true, full_scan);

        // 単一ビットマスク: 対象列だけが値化され、他はすべて None。
        for wanted_index in 1..schema.columns.len() {
            let mut mask = vec![false; schema.columns.len()];
            mask[wanted_index] = true;
            let scanned =
                scan_scalar_columns_masked(&schema, &buf, Some(&mask)).expect("scan single-bit");
            for (idx, slot) in scanned.iter().enumerate() {
                if idx == wanted_index {
                    assert_eq!(
                        *slot, full_scan[idx],
                        "column {idx} must decode its value when masked wanted"
                    );
                } else {
                    assert_eq!(*slot, None, "column {idx} must be None when not wanted");
                }
            }
        }
    }

    #[test]
    fn validate_scalar_columns_accepts_all_scalar_types_row() {
        let schema = all_scalar_types_schema();
        let values = all_scalar_types_values();
        let buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        assert!(validate_scalar_columns(&schema, &buf).is_ok());
    }

    // --- 非要求列（マスク false）でも構造・値域検証を省略しない ------------------

    #[test]
    fn masked_unwanted_real_nan_is_rejected() {
        // 有限値で符号化した後、REAL の値バイト列（presence 直後の 4 バイト）を
        // NaN のビットパターンへ直接書き換える（`encode_scalar_columns` 自身は
        // 非有限値を拒否するため経由できない。decode 側の検証を独立に固定する）。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("score", ColumnType::Real, false)],
        );
        let mut buf = encode_scalar_columns(&schema, &[Value::Real(1.0)]).expect("encode");
        let value_offset = buf.len() - 4;
        buf[value_offset..].copy_from_slice(&f32::NAN.to_le_bytes());
        let mask = [false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
        assert!(matches!(
            validate_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn masked_unwanted_boolean_unknown_byte_is_rejected() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("flag", ColumnType::Boolean, false)],
        );
        // BOOL_FALSE_BYTE=0, BOOL_TRUE_BYTE=1 以外の未知バイト。
        let buf = vec![PRESENCE_VALUE, 2];
        let mask = [false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn masked_unwanted_numeric_precision_overflow_is_rejected() {
        // `precision=3` のスキーマへ、桁数がそれを超える unscaled 値を直接書き込む
        // （`encode_scalar_columns` は `fits_precision` を検証するため経由できない）。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "n",
                ColumnType::Numeric {
                    precision: 3,
                    scale: 0,
                },
                false,
            )],
        );
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        let unscaled: i128 = 12_345; // precision=3 (999 が上限) を超える。
        buf.extend_from_slice(&unscaled.to_le_bytes());
        let mask = [false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn masked_unwanted_integer_truncated_is_rejected() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("n", ColumnType::Integer, false)],
        );
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        buf.extend_from_slice(&[0u8, 1u8]); // 4 バイト必要だが 2 バイトで打ち切り。
        let mask = [false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn masked_unwanted_date_out_of_range_is_rejected() {
        // 日時の値域検証（`crate::datetime::validate_date_days`）も、非要求列
        // （マスク false）で省略しない。`encode_scalar_columns` は範囲外の
        // `Value::Date` を拒否するため、decode 側の検証を独立に固定するには
        // バイト列を直接組み立てる必要がある。
        let schema = TableSchema::new("docs", vec![ColumnDef::new("dt", ColumnType::Date, false)]);
        let mut buf = Vec::new();
        buf.push(PRESENCE_VALUE);
        let out_of_range_days = crate::datetime::DATE_MAX_DAYS + 1;
        buf.extend_from_slice(&out_of_range_days.to_le_bytes());
        let mask = [false];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn masked_unwanted_all_scalar_types_trailing_bytes_are_rejected() {
        let schema = all_scalar_types_schema();
        let values = all_scalar_types_values();
        let mut buf = encode_scalar_columns(&schema, &values).expect("encode scalar");
        buf.push(0xff);
        let all_false = vec![false; schema.columns.len()];
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&all_false)),
            Err(RowCodecError::Invalid(_))
        ));
        assert!(matches!(
            validate_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
    }

    // --- ADD COLUMN 後の既存行の DEFAULT 読み出し時補完（Issue #1169） ---------

    use crate::catalog::ColumnDefault;

    /// 旧スキーマ（`a TEXT NOT NULL`）で書いた行と、`extra` を末尾追加した新スキーマ。
    fn old_row_and_extended_schema(extra: Vec<ColumnDef>) -> (Vec<u8>, TableSchema) {
        let old = TableSchema::new("t", vec![ColumnDef::new("a", ColumnType::Text, false)]);
        let buf = encode_scalar_columns(&old, &[Value::Text("x".to_string())]).expect("encode");
        let mut columns = old.columns.clone();
        columns.extend(extra);
        (buf, TableSchema::new("t", columns))
    }

    fn default_cols() -> Vec<ColumnDef> {
        vec![
            ColumnDef::new("t", ColumnType::Text, true)
                .with_default(ColumnDefault::Text("dt".to_string())),
            ColumnDef::new("i", ColumnType::Integer, false)
                .with_default(ColumnDefault::Number("7".to_string())),
            ColumnDef::new("b", ColumnType::BigInt, true)
                .with_default(ColumnDefault::Number("-9".to_string())),
            ColumnDef::new("r", ColumnType::Real, true)
                .with_default(ColumnDefault::Number("1.5".to_string())),
            ColumnDef::new("d", ColumnType::Double, true)
                .with_default(ColumnDefault::Number("2.5".to_string())),
            ColumnDef::new(
                "n",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                false,
            )
            .with_default(ColumnDefault::Number("3.14".to_string())),
            ColumnDef::new("f", ColumnType::Boolean, false).with_default(ColumnDefault::Bool(true)),
        ]
    }

    #[test]
    fn missing_trailing_columns_with_default_read_as_default_in_scan_and_decode() {
        let (buf, schema) = old_row_and_extended_schema(default_cols());
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(scanned[1], Some(ScalarRef::Text("dt")));
        assert_eq!(scanned[2], Some(ScalarRef::Integer(7)));
        assert_eq!(scanned[3], Some(ScalarRef::BigInt(-9)));
        assert_eq!(scanned[4], Some(ScalarRef::Real(1.5)));
        assert_eq!(scanned[5], Some(ScalarRef::Double(2.5)));
        assert!(matches!(scanned[6], Some(ScalarRef::Numeric(_))));
        assert_eq!(scanned[7], Some(ScalarRef::Bool(true)));

        let decoded = decode_scalar_columns(&schema, &buf).expect("decode");
        assert_eq!(decoded[1], Value::Text("dt".to_string()));
        assert_eq!(decoded[2], Value::Integer(7));
        assert_eq!(decoded[7], Value::Bool(true));
        validate_scalar_columns(&schema, &buf).expect("validate");
    }

    #[test]
    fn masked_scan_returns_none_for_unwanted_default_column() {
        let (buf, schema) = old_row_and_extended_schema(default_cols());
        let mut mask = vec![false; schema.columns.len()];
        mask[2] = true;
        let scanned = scan_scalar_columns_masked(&schema, &buf, Some(&mask)).expect("scan");
        assert_eq!(scanned[1], None);
        assert_eq!(scanned[2], Some(ScalarRef::Integer(7)));
    }

    /// `DATE` 列の既定値（Issue #1279）。scan・v1 decode の両方で補完され、
    /// 束縛できない値はカタログ破損相当として fail-closed に拒否する。
    #[test]
    fn date_default_is_filled_and_invalid_date_default_is_rejected() {
        let days = crate::datetime::parse_date("2020-01-02").expect("date");
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("dt", ColumnType::Date, false)
                .with_default(ColumnDefault::Text("2020-01-02".to_string()))]);
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(scanned[1], Some(ScalarRef::Date(days)));
        let decoded = decode_scalar_columns(&schema, &buf).expect("decode");
        assert_eq!(decoded[1], Value::Date(days));

        for bad in ["2020-02-30", "abc"] {
            let (buf, schema) =
                old_row_and_extended_schema(vec![ColumnDef::new("dt", ColumnType::Date, true)
                    .with_default(ColumnDefault::Text(bad.to_string()))]);
            assert!(
                matches!(
                    scan_scalar_columns(&schema, &buf),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad}"
            );
            // マスク外でも検証する。
            let mask = vec![false; schema.columns.len()];
            assert!(
                matches!(
                    scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad} (masked out)"
            );
        }
    }

    /// `TIMESTAMP` 列の既定値（Issue #1280）。scan・v1 decode の両方で補完され、
    /// 束縛できない値はカタログ破損相当として fail-closed に拒否する。
    #[test]
    fn timestamp_default_is_filled_and_invalid_timestamp_default_is_rejected() {
        let micros = crate::datetime::parse_timestamp("2020-01-02 03:04:05.123456").expect("ts");
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("ts", ColumnType::Timestamp, false)
                .with_default(ColumnDefault::Text(
                    "2020-01-02 03:04:05.123456".to_string(),
                ))]);
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(scanned[1], Some(ScalarRef::Timestamp(micros)));
        let decoded = decode_scalar_columns(&schema, &buf).expect("decode");
        assert_eq!(decoded[1], Value::Timestamp(micros));

        for bad in ["2020-02-30 00:00:00", "abc"] {
            let (buf, schema) = old_row_and_extended_schema(vec![ColumnDef::new(
                "ts",
                ColumnType::Timestamp,
                true,
            )
            .with_default(ColumnDefault::Text(bad.to_string()))]);
            assert!(
                matches!(
                    scan_scalar_columns(&schema, &buf),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad}"
            );
            let mask = vec![false; schema.columns.len()];
            assert!(
                matches!(
                    scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad} (masked out)"
            );
        }
    }

    #[test]
    fn default_scalar_maps_timestamp_errors() {
        assert_eq!(
            default_scalar(
                &ColumnType::Timestamp,
                &ColumnDefault::Text("2020-02-30 00:00:00".into())
            ),
            Err(DefaultBindError::DatetimeOverflow)
        );
        assert_eq!(
            default_scalar(&ColumnType::Timestamp, &ColumnDefault::Text("abc".into())),
            Err(DefaultBindError::DatetimeFormat)
        );
        assert_eq!(
            default_scalar(&ColumnType::Timestamp, &ColumnDefault::Number("1".into())),
            Err(DefaultBindError::Incompatible)
        );
    }

    #[test]
    fn default_scalar_maps_date_errors() {
        assert_eq!(
            default_scalar(&ColumnType::Date, &ColumnDefault::Text("2020-02-30".into())),
            Err(DefaultBindError::DatetimeOverflow)
        );
        assert_eq!(
            default_scalar(&ColumnType::Date, &ColumnDefault::Text("abc".into())),
            Err(DefaultBindError::DatetimeFormat)
        );
        assert_eq!(
            default_scalar(&ColumnType::Date, &ColumnDefault::Number("1".into())),
            Err(DefaultBindError::Incompatible)
        );
    }

    /// `UUID` 列の既定値（Issue #1281）。scan・v1 decode の両方で補完され、
    /// 束縛できない値はカタログ破損相当として fail-closed に拒否する。
    #[test]
    fn uuid_default_is_filled_and_invalid_uuid_default_is_rejected() {
        let text = "0A0B0C0D-0000-0000-0000-00000000000F";
        let uuid = crate::uuid::parse_uuid_text(text).expect("uuid");
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("u", ColumnType::Uuid, false)
                .with_default(ColumnDefault::Text(text.to_string()))]);
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(scanned[1], Some(ScalarRef::Uuid(uuid)));
        let decoded = decode_scalar_columns(&schema, &buf).expect("decode");
        assert_eq!(decoded[1], Value::Uuid(uuid));

        for bad in ["abc", "00000000000000000000000000000001"] {
            let (buf, schema) =
                old_row_and_extended_schema(vec![ColumnDef::new("u", ColumnType::Uuid, true)
                    .with_default(ColumnDefault::Text(bad.to_string()))]);
            assert!(
                matches!(
                    scan_scalar_columns(&schema, &buf),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad}"
            );
            let mask = vec![false; schema.columns.len()];
            assert!(
                matches!(
                    scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
                    Err(RowCodecError::Invalid(_))
                ),
                "{bad} (masked out)"
            );
        }
    }

    #[test]
    fn default_scalar_maps_uuid_errors() {
        assert_eq!(
            default_scalar(&ColumnType::Uuid, &ColumnDefault::Text("abc".into())),
            Err(DefaultBindError::Malformed)
        );
        assert_eq!(
            default_scalar(&ColumnType::Uuid, &ColumnDefault::Number("1".into())),
            Err(DefaultBindError::Incompatible)
        );
        let upper_default = ColumnDefault::Text("ABCDEF00-0000-0000-0000-000000000000".into());
        let lower_default = ColumnDefault::Text("abcdef00-0000-0000-0000-000000000000".into());
        let upper = default_scalar(&ColumnType::Uuid, &upper_default);
        let lower = default_scalar(&ColumnType::Uuid, &lower_default);
        assert!(matches!(upper, Ok(ScalarRef::Uuid(_))));
        assert_eq!(upper, lower);
    }

    #[test]
    fn missing_non_nullable_column_without_default_is_still_rejected() {
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("z", ColumnType::Text, false)]);
        assert!(matches!(
            scan_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn missing_column_with_unbindable_default_is_rejected_fail_closed() {
        // カタログ破損相当（束縛できない DEFAULT）は NULL へ黙って落とさず拒否する。
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("z", ColumnType::Integer, true)
                .with_default(ColumnDefault::Number("abc".to_string()))]);
        assert!(matches!(
            scan_scalar_columns(&schema, &buf),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn unbindable_default_is_rejected_even_when_masked_out() {
        // マスク外（読まない列）の不正 DEFAULT も検出する。参照列の選択で
        // カタログ破損の検出結果が変わらないこと（fail-closed）を固定する。
        let (buf, schema) =
            old_row_and_extended_schema(vec![ColumnDef::new("z", ColumnType::Integer, true)
                .with_default(ColumnDefault::Number("abc".to_string()))]);
        let mask = vec![true, false];
        assert_eq!(mask.len(), schema.columns.len());
        assert!(matches!(
            scan_scalar_columns_masked(&schema, &buf, Some(&mask)),
            Err(RowCodecError::Invalid(_))
        ));
    }

    #[test]
    fn explicit_null_is_not_replaced_by_default() {
        let old = TableSchema::new(
            "t",
            vec![ColumnDef::new("a", ColumnType::Text, true)
                .with_default(ColumnDefault::Text("dflt".to_string()))],
        );
        let buf = encode_scalar_columns(&old, &[Value::Null]).expect("encode");
        let scanned = scan_scalar_columns(&old, &buf).expect("scan");
        assert_eq!(scanned[0], None);
    }

    #[test]
    fn update_of_old_row_materializes_default() {
        let (buf, schema) = old_row_and_extended_schema(default_cols());
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        let new_a = Value::Text("y".to_string());
        let merged = merge_encode_scalar_columns(&schema, &scanned, &[(0, &new_a)]).expect("merge");
        let decoded = decode_scalar_columns(&schema, &merged).expect("decode");
        assert_eq!(decoded[0], Value::Text("y".to_string()));
        assert_eq!(decoded[1], Value::Text("dt".to_string()));
        assert_eq!(decoded[2], Value::Integer(7));
        // 物理化後は末尾欠落ではなく明示値としてバッファに載っている。
        assert!(merged.len() > buf.len());
    }

    #[test]
    fn v1_decode_row_fills_default_for_missing_columns() {
        let old = TableSchema::new("t", vec![ColumnDef::new("a", ColumnType::Text, false)]);
        let buf = encode_row(
            &old,
            "tenant-a",
            Visibility::Public,
            &[Value::Text("x".to_string())],
        )
        .expect("encode");
        let mut columns = old.columns.clone();
        columns.push(
            ColumnDef::new("i", ColumnType::Integer, false)
                .with_default(ColumnDefault::Number("7".to_string())),
        );
        let schema = TableSchema::new("t", columns);
        let decoded = decode_row(&schema, &buf).expect("decode");
        assert_eq!(decoded.values[1], Value::Integer(7));
    }

    #[test]
    fn nullable_column_without_default_before_default_column_stays_null() {
        // DEFAULT なしの nullable 列は NULL のまま、後続の DEFAULT 列だけに既定値が
        // 補われる（列ごとに独立して判定される）。
        let (buf, schema) = old_row_and_extended_schema(vec![
            ColumnDef::new("mid", ColumnType::Text, true),
            ColumnDef::new("i", ColumnType::Integer, false)
                .with_default(ColumnDefault::Number("7".to_string())),
        ]);
        // buf は `a` のみ。`mid` は nullable 欠落 → NULL、`i` は既定値。
        let scanned = scan_scalar_columns(&schema, &buf).expect("scan");
        assert_eq!(scanned[1], None);
        assert_eq!(scanned[2], Some(ScalarRef::Integer(7)));
    }
}
