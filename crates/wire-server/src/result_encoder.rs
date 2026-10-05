//! 簡易クエリプロトコルの応答メッセージ（`RowDescription`/`DataRow`/
//! `CommandComplete`/`EmptyQueryResponse`）のバイト列生成を担う。
//!
//! 責務境界: 本モジュールは純関数のみを持ち、I/O を一切行わない
//! （`Vec<u8>` を返すのみ）。呼び出し元の [`crate::simple_query`] が
//! `TcpStream` への書き込みを担当する。`engine::sql::exec::{ColumnMeta, Cell,
//! ResultRow}` から wire v3 の text/binary フォーマットへの写像がここに閉じる
//! （TASK-73・WIRE-1・WIRE-14）。
//!
//! 型写像。単一情報源は [`WireType`]（本モジュール `pub(crate)`）で、
//! `crate::http::query::response`（NoSQL 表層の JSON 応答スキーマ
//! `columns[].type`。Issue #762・NOSQL-11）もこの enum を経由して同じ写像を
//! 参照する（2 箇所に写像表を持たない）:
//! - `ColumnMeta::Id` → `numeric`（OID 1700, typlen -1）。engine の行 ID は
//!   `u64` 全域（`u64::MAX` を含む）を有効値とするため、符号付き 64bit の
//!   `int8`（OID 20）では `i64::MAX` を超える正当な ID を表現できない
//!   （PR #210 レビュー指摘）。10進テキスト表現は `int8`/`numeric` のどちらでも
//!   同一であり、`numeric` として公告することで値域制限なく全 `u64` 値を
//!   そのまま送出できる
//! - `ColumnMeta::Scalar{ty: Text}` → `text`（OID 25, typlen -1）
//! - `ColumnMeta::Scalar{ty: Vector(_)}` → `text`（OID 25。値は `[v1,v2,...]` 形式。
//!   `VECTOR` 独自のバイナリ表現は定義しないため引き続き text 固定・受入基準 3）
//! - `ColumnMeta::Scalar{ty: Boolean}` → `bool`（OID 16, typlen 1。値は `t`/`f`。
//!   WIRE-13・TASK-200・Issue #895）
//! - `ColumnMeta::Scalar{ty: Integer}` → `int4`（OID 23, typlen 4。TABLE-13・
//!   TASK-196・Issue #881・#903 レビュー指摘。`Cell::SignedInteger` が送出する
//!   10進テキストは `int4` の表現とそのまま一致する）
//! - `ColumnMeta::Scalar{ty: BigInt}` → `int8`（OID 20, typlen 8。同上）
//! - `ColumnMeta::Scalar{ty: Real}` → `float4`（OID 700, typlen 4。WIRE-13・
//!   TASK-200・Issue #895）
//! - `ColumnMeta::Scalar{ty: Double}` → `float8`（OID 701, typlen 8。同上）
//! - `ColumnMeta::Scalar{ty: Date}` → `date`（OID 1082, typlen 4。同上）
//! - `ColumnMeta::Scalar{ty: Timestamp}` → `timestamp`（OID 1114, typlen 8。同上）
//! - `ColumnMeta::Scalar{ty: Bytea}` → `bytea`（OID 17, typlen -1。同上）
//! - `ColumnMeta::Scalar{ty: Uuid}` → `uuid`（OID 2950, typlen 16。同上）
//! - `ColumnMeta::Scalar{ty: Json}` → `json`（OID 114, typlen -1。同上）
//! - `ColumnMeta::Scalar{ty: Jsonb}` → `jsonb`（OID 3802, typlen -1。同上）
//! - `ColumnMeta::Scalar{ty: Array(_)}`／`Scalar{ty: Enum(_)}` → `text`
//!   （OID 25。専用 OID は未策定のまま `text` 表現を公告する。受入基準 3
//!   の対象外＝据え置き判断。WIRE-13・TASK-200・Issue #895）
//! - `ColumnMeta::Scalar{ty: Numeric{..}}` → `numeric`（OID 1700, typlen -1。
//!   値は `Cell::Numeric`（`Decimal` の正規テキスト）のテキスト表現。
//!   バイナリ形式は `supports_binary` が別途 fail-closed に非対応とする
//!   〔TASK-197・Issue #885〕）
//! - `ColumnMeta::Computed{ty: Some(t)}` → `Scalar{ty: t}` と同じ写像（集計結果・
//!   式列の静的な結果型。`COUNT` → `int8`・`SUM(REAL)` → `float4` など。Issue #1173・
//!   WIRE-13・TABLE-13 のポインタ）。`Computed{ty: None}`（ベクトル式など静的型を
//!   持たない列）は `text`（OID 25）へフォールバックする
//! - `REAL`（`float4`）列のテキスト値は PostgreSQL の float4 出力形式
//!   （`0.1` → `0.1`・`1e6` → `1e+06`。[`engine::scalar_float::format_real`]）で出す
//!   （[`cell_to_text_for_column`]）。`DOUBLE PRECISION` は float8 出力形式
//!   （`1e20` → `1e+20`。[`engine::scalar_float::format_double`]。Issue #1343）
//!
//! バイナリ形式（format code 1・WIRE-14・TASK-218・Issue #936）: 列ごとに
//! テキスト／バイナリを要求できる（PostgreSQL の Bind 規則。[`ResultFormats`]）。
//! 対応型は `TEXT`（UTF-8 生バイト）と、Issue #1172 で追加した `INTEGER`・
//! `BIGINT`・`REAL`・`DOUBLE PRECISION`・`BOOLEAN`・`BYTEA`・`UUID`（PostgreSQL の
//! send 形式。`Computed{ty: Some(t)}`〔集計・式列・ウィンドウ関数の結果列〕も `t` が
//! 対応型ならバイナリ可。Issue #1407。結線は
//! [`encode_data_row_into_with_columns`]）で、`Id`（`numeric`）・
//! `Vector`（text 表現だが値はベクトル）・静的型を持たない `Computed{ty: None}` は
//! バイナリ非対応として事前検査（[`validate_binary_formats`]）で
//! `0A000` に拒否する（spec の WIRE-14 が定める対応範囲。`VECTOR` 列の独自
//! バイナリ表現は定義しない）。`NUMERIC`・`DATE`・
//! `TIMESTAMP`・`JSON`・`JSONB`・配列・`ENUM` も `0A000` のまま。
//!
//! Bind の結果形式コードから本 API への結線は
//! [`crate::extended_query::handle_bind`]（[`ResultFormats::resolve`]／
//! [`validate_binary_formats`] を呼ぶ）・Describe(Portal) の
//! `RowDescription`（[`encode_row_description_with_formats`]）・Execute の
//! `DataRow`（[`encode_data_row_into_with_formats`]）が担う（#934・WIRE-11・
//! WIRE-14）。パラメータ側（Bind が受け取る `$n` の binary 入力）は
//! `$n` 束縛そのものが WIRE-12・#935 未実装のため対象外のまま。
//!
//! サイズ安全: フレーム長は `i32::try_from`/`checked_add` で算出し、超過は
//! `Err(EncodeError::FrameTooLarge)` とする（`.claude/rules/coding-rust.md`
//! 「untrusted 入力の扱い」。応答側だが同じ規律を踏襲し `as i32` によるオーバー
//! フローを避ける）。

use engine::error_format::ErrorClass;
use engine::sql::exec::{Cell, ColumnMeta, ResultRow};

/// 応答メッセージ組み立て時のフレーム長超過エラー。呼び出し元
/// （[`crate::simple_query`]）はこれを内部エラー（`XX000`）として扱い、panic
/// させない。
#[derive(Debug)]
pub struct EncodeError;

/// 結果列 1 個の応答形式（PostgreSQL Bind の format code。WIRE-14）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatCode {
    Text,
    Binary,
}

/// バイナリ形式（WIRE-14）の解決・事前検査で生じる分類済みエラー。
/// `error_class()` で `engine::error_format::ErrorClass` へ写像し、呼び出し元
/// （`crate::simple_query`／Bind 結線先の #934）が `wire_code` を一意に決定
/// できるようにする。メッセージには要求者自身の列番号までしか含めない
/// （テーブル名・他テナントの存在情報を含めない。`.claude/rules/security.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormatError {
    /// 形式コードの個数が列数と一致しない（0 個・1 個・列数と同数のいずれでもない）。
    FormatCountMismatch,
    /// 形式コードの値が {0, 1} 以外。
    InvalidFormatCode,
    /// 指定列がバイナリ形式に非対応（[`column_binary_support`] が `false`）。
    UnsupportedType { column_index: usize },
}

impl BinaryFormatError {
    /// `wire_code` 分類。個数不一致・不正値は untrusted な Bind 入力の構文
    /// 違反として `08P01`（`ProtocolViolation`）、非対応型は fail-closed の
    /// 意図的な拒否として `0A000`（`FeatureNotSupported`）に写像する。
    ///
    /// PostgreSQL 本体は不正な format code 値を `22023` で返すが、本リポの
    /// `ErrorClass::OperationIdContentMismatch` は `22023` を専有しており
    /// 意味が異なるため、`ErrorClass` を増やさず本モジュールの fail-closed
    /// 方針（構文違反は `08P01`）に寄せる（設計判断の詳細は
    /// `docs/design/wire-binary-format.md` 参照）。
    pub const fn error_class(self) -> ErrorClass {
        match self {
            BinaryFormatError::FormatCountMismatch => ErrorClass::ProtocolViolation,
            BinaryFormatError::InvalidFormatCode => ErrorClass::ProtocolViolation,
            BinaryFormatError::UnsupportedType { .. } => ErrorClass::FeatureNotSupported,
        }
    }
}

/// Bind が送る結果形式コード列（PostgreSQL の規約。WIRE-14）。列ごとへの解決
/// （[`ResultFormats::resolve`]）までを本型に閉じ、解決結果 `Vec<FormatCode>`
/// の長さは列数（`i16` で有界）に比例するのみで、`codes` スライス自体の長さ
/// 上限検証は untrusted な Bind 本文を解析する側（#934）の責務とする
/// （本型は受け取ったスライス長に比例した確保を行わない）。
#[derive(Debug, Clone, Copy)]
pub struct ResultFormats<'a> {
    codes: &'a [i16],
}

impl<'a> ResultFormats<'a> {
    /// Bind から受け取った形式コード列をそのまま保持する。
    pub const fn new(codes: &'a [i16]) -> Self {
        ResultFormats { codes }
    }

    /// 列ごとの [`FormatCode`] へ解決する（PostgreSQL の Bind 規則）。
    /// - 0 個 → 全列 [`FormatCode::Text`]
    /// - 1 個 → 全列に同じ値を適用
    /// - 列数と同数 → 列ごとに適用
    /// - それ以外の個数 → [`BinaryFormatError::FormatCountMismatch`]
    /// - 値が {0, 1} 以外 → [`BinaryFormatError::InvalidFormatCode`]
    pub fn resolve(&self, column_count: usize) -> Result<Vec<FormatCode>, BinaryFormatError> {
        let to_format = |raw: i16| -> Result<FormatCode, BinaryFormatError> {
            match raw {
                0 => Ok(FormatCode::Text),
                1 => Ok(FormatCode::Binary),
                _ => Err(BinaryFormatError::InvalidFormatCode),
            }
        };
        match self.codes.len() {
            0 => Ok(vec![FormatCode::Text; column_count]),
            1 => {
                // `get(0)` で明示的に取得する（untrusted 由来スライスへの `[]` 直接
                // アクセスを避ける規律。`.claude/rules/coding-rust.md`）。
                let raw = *self
                    .codes
                    .first()
                    .ok_or(BinaryFormatError::FormatCountMismatch)?;
                let format = to_format(raw)?;
                Ok(vec![format; column_count])
            }
            n if n == column_count => self.codes.iter().copied().map(to_format).collect(),
            _ => Err(BinaryFormatError::FormatCountMismatch),
        }
    }
}

/// バイナリ形式を要求された列がすべて対応型であることを、`RowDescription` を
/// 送出する前に検査する（WIRE-14）。1 バイトも送る前に判定することで、応答
/// 列の一部を送ってからエラーにする（フレーム崩壊）事態を避ける。
///
/// 判定は公告型（[`ColumnMeta`]／[`WireType`]）で静的に行い、実行時の
/// `Cell` は参照しない（型公告と実値の不一致を防ぐ）。
///
/// `columns.len() != formats.len()` は本来 [`ResultFormats::resolve`] の
/// 呼び出し規約違反（Bind 形式コードの解決結果が列数と食い違う）だが、
/// `.zip()` は短い方に合わせて黙って打ち切るため、この不一致をここで
/// 検査しないと後段のエンコーダまで持ち越され `EncodeError`（`XX000`）
/// として現れてしまう。入力由来の不整合は `08P01`
/// （[`BinaryFormatError::FormatCountMismatch`]）として、事前検査という
/// 本関数の契約どおりここで拒否する。
pub fn validate_binary_formats(
    columns: &[ColumnMeta],
    formats: &[FormatCode],
) -> Result<(), BinaryFormatError> {
    if columns.len() != formats.len() {
        return Err(BinaryFormatError::FormatCountMismatch);
    }
    for (index, (meta, format)) in columns.iter().zip(formats.iter()).enumerate() {
        if matches!(format, FormatCode::Binary) && !column_binary_support(meta) {
            return Err(BinaryFormatError::UnsupportedType {
                column_index: index,
            });
        }
    }
    Ok(())
}

/// PostgreSQL の send 関数と同じレイアウトで型ごとのバイナリ表現を組み立てる
/// 純関数群（WIRE-14）。本 Issue で `WireType` 側に結線されるのは `text`
/// （[`text`]）に加え、数値・真偽値・bytea・uuid（Issue #1172）を
/// [`super::encode_data_row_into_with_columns`] が結線する。
pub mod binary {
    /// `int4` のバイナリ表現（4 バイト・ビッグエンディアン 2 の補数）。
    pub fn int4(v: i32) -> [u8; 4] {
        v.to_be_bytes()
    }

    /// `int8` のバイナリ表現（8 バイト・ビッグエンディアン 2 の補数）。
    pub fn int8(v: i64) -> [u8; 8] {
        v.to_be_bytes()
    }

    /// `float4` のバイナリ表現（IEEE 754 単精度のビット列をそのままビッグ
    /// エンディアンで送る。NaN／±Inf もビット列のまま送出する）。
    pub fn float4(v: f32) -> [u8; 4] {
        v.to_bits().to_be_bytes()
    }

    /// `float8` のバイナリ表現（IEEE 754 倍精度のビット列。float4 と同じ方針）。
    pub fn float8(v: f64) -> [u8; 8] {
        v.to_bits().to_be_bytes()
    }

    /// `bool` のバイナリ表現（1 バイト。`true` → `0x01`、`false` → `0x00`）。
    pub fn bool_(v: bool) -> [u8; 1] {
        [if v { 1 } else { 0 }]
    }

    /// `bytea` のバイナリ表現（生バイトをそのまま返す。長さ付与は呼び出し元）。
    pub fn bytea(v: &[u8]) -> &[u8] {
        v
    }

    /// `uuid` のバイナリ表現（16 バイトをそのまま返す）。
    pub fn uuid(v: [u8; 16]) -> [u8; 16] {
        v
    }

    /// `text` のバイナリ表現（UTF-8 生バイト。PostgreSQL の text send と同一。
    /// [`super::encode_data_row_into_with_formats`] が結線する唯一の型）。
    pub fn text(v: &str) -> &[u8] {
        v.as_bytes()
    }
}

/// [`WireType`] の enum 定義と [`WireType::ALL`]（全 variant 列挙）を単一の
/// variant トークン列から同時生成するマクロ。手動保守の配列に variant を
/// 追加し忘れる余地を構造的に無くす（PR #1037 codex-review 指摘の是正。
/// 旧実装は `ALL: [WireType; 13]` という配列サイズの明示一致だけに頼って
/// おり、「variant を追加したのに `ALL` への追加を怠る」ケースでも配列長
/// リテラル `13` を書き換えなければコンパイル・[`wire_type_all_is_exhaustive`]
/// の双方を素通りしてしまっていた（`docs/design/wire-type-oid-mapping.md`
/// の同種の誤った保証の記載も本 Issue で修正済み）。`ALL` の要素は enum
/// 定義に渡した variant トークン列からそのまま生成されるため、enum へ
/// variant を追加すれば `ALL` にも必ず反映される。
macro_rules! wire_type_enum {
    (
        $(#[$enum_meta:meta])*
        enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $name {
            /// 全 variant の列挙（型写像表の単一情報源）。[`wire_type_enum!`]
            /// が enum 定義と同一の variant トークン列から生成するため、
            /// `ALL` だけを更新し忘れる状態が構造的に発生し得ない。
            /// [`from_oid`]・単体テストの網羅性検査はここから導出する。
            /// 単体テスト専用のため非テストビルドでは未使用となり
            /// `#[cfg(test)]` で dead_code を回避する。
            #[cfg(test)]
            pub(crate) const ALL: [$name; wire_type_enum!(@count $($variant)+)] =
                [$($name::$variant),+];
        }
    };
    (@count $($variant:tt)+) => {
        0usize $(+ wire_type_enum!(@one $variant))+
    };
    (@one $variant:tt) => {
        1usize
    };
}

wire_type_enum! {
    /// `RowDescription` が公告する PostgreSQL 型（型写像表の単一情報源。
    /// Issue #762・NOSQL-11）。`crate::http::query::response`（NoSQL 表層の JSON
    /// 応答スキーマ `columns[].type`）も本 enum を経由して同じ写像を参照し、
    /// wire 側 `RowDescription` と JSON `columns` の型名が乖離しない構造にする
    /// （2 箇所に写像表を持たない）。`#[deny(clippy::wildcard_enum_match_arm)]`
    /// を付けた網羅 `match`（`http/status.rs` と同方針）で
    /// [`ColumnMeta`] に variant が増えたら両表層が同時にコンパイルエラーになる。
    enum WireType {
        /// `ColumnMeta::Id`。engine の行 ID は `u64` 全域（`u64::MAX` を含む）を
        /// 有効値とするため、符号付き 64bit の `int8`（OID 20）では表現できない
        /// 値が生じる（PR #210 レビュー指摘）。`ColumnMeta::Scalar{ty:
        /// Numeric{..}}`（TASK-197・Issue #885）も同じ OID 1700 を公告する
        /// （値の実体は別途 `Cell::Numeric` の正規テキスト表現）。`id` を `int8`
        /// へ変更しない据え置き判断は WIRE-13・TASK-200・Issue #895 参照。
        Numeric,
        /// `ColumnMeta::Scalar{ty: Text}`／`Scalar{ty: Vector(_)}`／
        /// `Scalar{ty: Array(_)}`／`Scalar{ty: Enum(_)}`／`Computed{ty: None}`。
        /// 専用 OID を持たない型はすべてここへ写像する（WIRE-13・TASK-200・
        /// Issue #895 の据え置き判断）。
        Text,
        /// `ColumnMeta::Scalar{ty: Integer}`（`INTEGER`。TABLE-13・TASK-196・
        /// Issue #881・#903 レビュー指摘）。`i32` 全域を表現する。
        Int4,
        /// `ColumnMeta::Scalar{ty: BigInt}`（`BIGINT`。同上）。`i64` 全域を表現する。
        Int8,
        /// `ColumnMeta::Scalar{ty: Boolean}`（`BOOLEAN`。TABLE-13・TASK-196・
        /// Issue #883・WIRE-13・TASK-200・Issue #895）。`Cell::Bool` が送出する
        /// `t`/`f` は `bool` の text 表現とそのまま一致する。
        Bool,
        /// `ColumnMeta::Scalar{ty: Real}`（`REAL`。TABLE-13・TASK-196・Issue #882・
        /// WIRE-13・TASK-200・Issue #895）。単精度浮動小数点。
        Float4,
        /// `ColumnMeta::Scalar{ty: Double}`（`DOUBLE PRECISION`。同上）。倍精度。
        Float8,
        /// `ColumnMeta::Scalar{ty: Date}`（TABLE-13・TASK-197・Issue #884・
        /// WIRE-13・TASK-200・Issue #895）。テキスト表現は `engine::datetime` の
        /// ISO 表記（`YYYY-MM-DD`）。
        Date,
        /// `ColumnMeta::Scalar{ty: Timestamp}`（同上）。テキスト表現は
        /// `engine::datetime` の ISO 表記（`YYYY-MM-DD HH:MM:SS[.ffffff]`）。
        Timestamp,
        /// `ColumnMeta::Scalar{ty: Bytea}`（TABLE-13・TASK-197・Issue #886・
        /// WIRE-13・TASK-200・Issue #895）。テキスト表現は `\x` + 小文字 16 進
        /// （`engine::bytea::format_hex_text`。PostgreSQL 既定の `bytea_output=hex`
        /// と同形）。
        Bytea,
        /// `ColumnMeta::Scalar{ty: Uuid}`（TABLE-13〔検討中〕・TASK-197・
        /// Issue #887・WIRE-13・TASK-200・Issue #895）。テキスト表現は小文字
        /// `8-4-4-4-12`。
        Uuid,
        /// `ColumnMeta::Scalar{ty: Json}`（TABLE-14・TASK-198・Issue #889・
        /// WIRE-13・TASK-200・Issue #895）。テキスト表現は格納テキストそのまま。
        Json,
        /// `ColumnMeta::Scalar{ty: Jsonb}`（同上）。テキスト表現は正規化済み
        /// テキスト。
        Jsonb,
    }
}

impl WireType {
    /// PostgreSQL 型 OID（`pg_type.oid`）。網羅 `match`（ワイルドカード腕
    /// なし）のため `WireType` に variant が増えると本関数はコンパイル
    /// エラーで検出する。
    pub(crate) const fn oid(self) -> i32 {
        match self {
            WireType::Numeric => 1700,
            WireType::Text => 25,
            WireType::Int4 => 23,
            WireType::Int8 => 20,
            WireType::Bool => 16,
            WireType::Float4 => 700,
            WireType::Float8 => 701,
            WireType::Date => 1082,
            WireType::Timestamp => 1114,
            WireType::Bytea => 17,
            WireType::Uuid => 2950,
            WireType::Json => 114,
            WireType::Jsonb => 3802,
        }
    }

    /// 型長（`pg_type.typlen`）。可変長型は `-1`、固定長型は対応バイト数。
    pub(crate) const fn typlen(self) -> i16 {
        match self {
            WireType::Numeric
            | WireType::Text
            | WireType::Bytea
            | WireType::Json
            | WireType::Jsonb => -1,
            WireType::Int4 | WireType::Float4 | WireType::Date => 4,
            WireType::Int8 | WireType::Float8 | WireType::Timestamp => 8,
            WireType::Bool => 1,
            WireType::Uuid => 16,
        }
    }

    /// PostgreSQL 型名（`pg_type.typname`）。JSON 応答スキーマ
    /// （`crate::http::query::response`）の `columns[].type` が使う。
    pub(crate) const fn pg_type_name(self) -> &'static str {
        match self {
            WireType::Numeric => "numeric",
            WireType::Text => "text",
            WireType::Int4 => "int4",
            WireType::Int8 => "int8",
            WireType::Bool => "bool",
            WireType::Float4 => "float4",
            WireType::Float8 => "float8",
            WireType::Date => "date",
            WireType::Timestamp => "timestamp",
            WireType::Bytea => "bytea",
            WireType::Uuid => "uuid",
            WireType::Json => "json",
            WireType::Jsonb => "jsonb",
        }
    }

    /// OID からの逆引き。[`ALL`] を線形走査するのみで、逆引き専用の第 2 の
    /// 写像表は持たない（単一情報源。WIRE-13・TASK-200・Issue #895）。
    /// 単体テスト専用（結合テスト側は独立の固定表を持つ）ため非テスト
    /// ビルドでは未使用となり `#[cfg(test)]` で dead_code を回避する。
    #[cfg(test)]
    pub(crate) fn from_oid(oid: i32) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.oid() == oid)
    }

    /// この型がバイナリ形式（format code 1）に対応するか（WIRE-14）。
    /// 網羅 `match`（`#[deny(clippy::wildcard_enum_match_arm)]`）にし、
    /// `WireType` に variant が増えたときにバイナリ可否の決定漏れを
    /// コンパイルエラーで検出する。
    #[deny(clippy::wildcard_enum_match_arm)]
    pub(crate) const fn supports_binary(self) -> bool {
        match self {
            // `id` は `numeric` として公告しており、WIRE-14 は `NUMERIC` を
            // 非対応型としている。
            WireType::Numeric => false,
            // `text` は UTF-8 生バイトがそのままバイナリ表現（PostgreSQL の
            // text send と同じ）。ただし `ColumnMeta::Scalar{ty: Vector(_)}`・
            // `Computed` は公告が text でも本来の値がベクトル／実行時型で
            // あるため、型そのものの対応可否とは別に列種別で判定する
            // （[`column_binary_support`] 参照）。
            WireType::Text => true,
            // 数値・真偽値・bytea・uuid は PostgreSQL の send 形式（ネット
            // ワークバイトオーダー）でバイナリ対応する（WIRE-14・TASK-218・
            // Issue #1172）。DataRow への結線は `encode_binary_cell`。
            WireType::Int4
            | WireType::Int8
            | WireType::Bool
            | WireType::Float4
            | WireType::Float8
            | WireType::Bytea
            | WireType::Uuid => true,
            // `DATE`／`TIMESTAMP`／`JSON`／`JSONB` は WIRE-14 の非対応型のまま
            // fail-closed で `0A000` に拒否する。
            WireType::Date | WireType::Timestamp | WireType::Json | WireType::Jsonb => false,
        }
    }
}

/// `ColumnMeta` 1 個がバイナリ形式に対応するか（WIRE-14）。`Id` は公告する
/// [`WireType`] の [`WireType::supports_binary`] へ委譲する（`numeric` のため非対応）。
/// `Scalar{ty}` と `Computed{ty: Some(ty)}`（集計結果・式列・ウィンドウ関数の静的な結果型。
/// Issue #1407）は同じ [`scalar_type_binary_support`] で判定する。静的型を持たない
/// `Computed{ty: None}`（ベクトル式など）は値の実体が実行時型で決まるため fail-closed で
/// 非対応とする（[`column_wire_type`] と同じ「網羅 `match` で variant 増加を
/// コンパイルエラーにする」方針を踏襲）。
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn column_binary_support(meta: &ColumnMeta) -> bool {
    match meta {
        ColumnMeta::Id => column_wire_type(meta).supports_binary(),
        ColumnMeta::Scalar { ty, .. } => scalar_type_binary_support(ty),
        ColumnMeta::Computed { ty: Some(ty), .. } => scalar_type_binary_support(ty),
        ColumnMeta::Computed { ty: None, .. } => false,
    }
}

/// `ColumnType` 1 個がバイナリ形式に対応するか（`Scalar`／`Computed{ty: Some}` の共有判定。
/// WIRE-14・TASK-218・Issue #1172・#1407）。対応型は `TEXT`・`INTEGER`・`BIGINT`・`REAL`・
/// `DOUBLE PRECISION`・`BOOLEAN`・`BYTEA`・`UUID`。値の実体（`Cell`）との整合は
/// `encode_binary_cell` が検査する。
#[deny(clippy::wildcard_enum_match_arm)]
fn scalar_type_binary_support(ty: &engine::catalog::ColumnType) -> bool {
    use engine::catalog::ColumnType;
    match ty {
        ColumnType::Text
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Boolean
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Bytea
        | ColumnType::Uuid => scalar_wire_type(ty).supports_binary(),
        // `VECTOR` は公告 OID が `text` でも値の実体がベクトルのため、独自の
        // バイナリ表現は定義せず `0A000` で拒否する（WIRE-14 決定）。
        // `ENUM`・`ARRAY` は専用 OID を持たない据え置き判断（Issue #895）、
        // `JSON`／`JSONB`・`NUMERIC`・`DATE`／`TIMESTAMP` はバイナリ表現が
        // 未策定のため、いずれも fail-closed で非対応とする。
        ColumnType::Vector(_)
        | ColumnType::Enum(_)
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb
        | ColumnType::Numeric { .. }
        | ColumnType::Date
        | ColumnType::Timestamp => false,
    }
}

/// `ColumnMeta` 1 個が公告する [`WireType`]（本モジュール先頭の型写像表を参照）。
/// `ColumnMeta::Scalar` は `ColumnType` ごとに写像を分ける（Issue #903 レビュー
/// 指摘: `INTEGER`／`BIGINT` を一律 `text` に写像すると `RowDescription`／NoSQL
/// 表層の JSON 応答が実際の値の型と乖離し、psql・ドライバ・ORM が整数列を
/// 文字列として扱ってしまう）。
#[deny(clippy::wildcard_enum_match_arm)]
pub(crate) fn column_wire_type(meta: &ColumnMeta) -> WireType {
    match meta {
        ColumnMeta::Id => WireType::Numeric, // u64 全域を表現するため int8 ではなく numeric
        ColumnMeta::Scalar { ty, .. } => scalar_wire_type(ty),
        // Issue #1173: 集計結果・式列は束縛時に確定した静的型の写像を共有する
        // （`Scalar` と同じ OID）。静的型を持たない列（ベクトル式）は `text`。
        ColumnMeta::Computed { ty, .. } => match ty {
            Some(t) => scalar_wire_type(t),
            None => WireType::Text,
        },
    }
}

/// `ColumnType` 1 個が公告する [`WireType`]（`Scalar`／`Computed{ty: Some}` の共有写像。
/// 本モジュール先頭の型写像表を参照）。
#[deny(clippy::wildcard_enum_match_arm)]
fn scalar_wire_type(ty: &engine::catalog::ColumnType) -> WireType {
    match ty {
        // `NUMERIC` 列（TABLE-13〔検討中〕・TASK-197、Issue #885）は値の実体が
        // `Cell::Numeric`（`Decimal` の正規テキスト）であり、`supports_binary`
        // が fail-closed に非対応とするのはバイナリ表現のみ。RowDescription／
        // HTTP 応答の型メタデータ（OID 1700・`"numeric"`）自体はテキスト形式
        // でも正しく公告できる（PR #1020 codex-review 指摘）。
        engine::catalog::ColumnType::Numeric { .. } => WireType::Numeric,
        // 以下は WIRE-13・TASK-200・Issue #895 の型写像表（本モジュール先頭
        // ドキュメント参照）。`ARRAY`・`ENUM` は専用 OID を持たない据え置き
        // 判断のため引き続き `text` を公告する。`VECTOR` も同様。
        engine::catalog::ColumnType::Text => WireType::Text,
        engine::catalog::ColumnType::Vector(_) => WireType::Text,
        engine::catalog::ColumnType::Boolean => WireType::Bool,
        engine::catalog::ColumnType::Integer => WireType::Int4,
        engine::catalog::ColumnType::BigInt => WireType::Int8,
        engine::catalog::ColumnType::Real => WireType::Float4,
        engine::catalog::ColumnType::Double => WireType::Float8,
        engine::catalog::ColumnType::Date => WireType::Date,
        engine::catalog::ColumnType::Timestamp => WireType::Timestamp,
        engine::catalog::ColumnType::Bytea => WireType::Bytea,
        engine::catalog::ColumnType::Uuid => WireType::Uuid,
        engine::catalog::ColumnType::Json => WireType::Json,
        engine::catalog::ColumnType::Jsonb => WireType::Jsonb,
        // `ARRAY`（Issue #888）・`ENUM`（Issue #890）は据え置き判断
        // （WIRE-13・TASK-200・Issue #895）により専用 OID を持たず、
        // text（`RowDescription`）で公告する。
        engine::catalog::ColumnType::Array(_) | engine::catalog::ColumnType::Enum(_) => {
            WireType::Text
        }
    }
}

pub(crate) fn column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::Scalar { name, .. } => name.as_str(),
        ColumnMeta::Computed { name, .. } => name.as_str(),
    }
}

/// メッセージ長フィールド（自身の 4 バイトを含む i32）を `checked` に計算する。
/// `crate::error_response`（TASK-153・ERR-1）も同じ `checked` 方式を踏襲するため
/// crate 内に限り公開する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub(crate) fn frame_len(body_len: usize) -> Result<i32, EncodeError> {
    let total = body_len.checked_add(4).ok_or(EncodeError)?;
    i32::try_from(total).map_err(|_| EncodeError)
}

/// `RowDescription`（'T'）を組み立てる。全列テキスト形式（format code 0）の
/// 薄いラッパーで、[`encode_row_description_with_formats`] を呼ぶ
/// （既存呼び出し元・テストとの互換のため残す。生成バイト列は完全に同一。
/// WIRE-14・Issue #936）。
pub fn encode_row_description(columns: &[ColumnMeta]) -> Result<Vec<u8>, EncodeError> {
    let formats = vec![FormatCode::Text; columns.len()];
    encode_row_description_with_formats(columns, &formats)
}

/// `RowDescription`（'T'）を列ごとの [`FormatCode`] を反映して組み立てる
/// （WIRE-14）。フィールド数は `i16` に収まる必要がある（カタログの列数上限で
/// 有界だが、念のため `try_from` で検証する）。`formats.len() != columns.len()`
/// は呼び出し元の内部不整合として `EncodeError`（`XX000`）とする
/// （untrusted 入力由来のバイナリ形式検査は [`validate_binary_formats`] が
/// 別途 `RowDescription` 送出前に済ませている前提）。
pub fn encode_row_description_with_formats(
    columns: &[ColumnMeta],
    formats: &[FormatCode],
) -> Result<Vec<u8>, EncodeError> {
    if columns.len() != formats.len() {
        return Err(EncodeError);
    }
    let field_count = i16::try_from(columns.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::new();
    body.extend_from_slice(&field_count.to_be_bytes());
    for (meta, format) in columns.iter().zip(formats.iter()) {
        let name = column_name(meta);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table OID
        body.extend_from_slice(&0i16.to_be_bytes()); // attr number
        let wire_type = column_wire_type(meta);
        body.extend_from_slice(&wire_type.oid().to_be_bytes());
        body.extend_from_slice(&wire_type.typlen().to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        let format_code: i16 = match format {
            FormatCode::Text => 0,
            FormatCode::Binary => 1,
        };
        body.extend_from_slice(&format_code.to_be_bytes());
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'T');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// 列の公告型を踏まえた `Cell` の text 表現（Issue #1173）。
///
/// `float4`（`REAL`・`SUM(REAL)` など）列の `Cell::Float` は f32 へ丸め済みの値を
/// 前提に PostgreSQL の float4 出力形式（[`engine::scalar_float::format_real`]）で出す。
/// f32 で表現できない f64 が来た場合は engine が丸めを忘れた内部不整合として
/// `EncodeError`（`XX000`）で fail-closed に拒否する（黙って丸めない）。それ以外は
/// [`cell_to_text`] と同じ。`DataRow`（[`encode_data_row_into`] 系）と
/// `COPY TO`（`crate::copy`）が共有する。
/// `pub(crate)`: `crate::copy`（Issue #939・WIRE-17）の `COPY (...) TO STDOUT`
/// 行エンコーダが、通常の `SELECT` 応答（`DataRow`）と同じ値表現を再利用する
/// ために公開する（COPY TO の出力と SELECT の text 出力が同一の cell 表現を
/// 共有することで、`COPY (...) TO STDOUT` の出力を同じテーブルへ
/// `COPY ... FROM STDIN` で再投入した際に値の往復が保たれる）。
pub(crate) fn cell_to_text_for_column(
    meta: &ColumnMeta,
    cell: &Cell,
) -> Result<Option<String>, EncodeError> {
    if let Cell::Float(f) = cell {
        if column_wire_type(meta) == WireType::Float4 {
            let narrowed = *f as f32;
            let exact = (narrowed.is_nan() && f.is_nan()) || f64::from(narrowed) == *f;
            if !exact {
                return Err(EncodeError);
            }
            return Ok(Some(engine::scalar_float::format_real(narrowed)));
        }
    }
    cell_to_text(cell)
}

/// `Cell` の text フォーマット表現。`Null` は `None`（`DataRow` の -1 長へ写像）。
///
/// `Cell::Integer` は `u64` 全域（`u64::MAX` を含む）を保持しうる。本モジュール
/// は `id` 列を `numeric`（OID 1700, 型写像表参照）として公告しており、
/// `numeric` の text 表現は符号無し10進整数をそのまま送出してよい（`int8` の
/// ような signed 64bit 制約が無い）ため、`i64` への変換は行わず値域制限なく
/// `to_string()` する（PR #210 レビュー指摘: 旧実装は `i64::try_from` で
/// `i64::MAX` 超の正当な ID を `EncodeError`/`XX000` にしていた）。
///
/// 列の型に依存しない基本表現で、[`cell_to_text_for_column`] が `float4` 以外の
/// 列で委譲する。
fn cell_to_text(cell: &Cell) -> Result<Option<String>, EncodeError> {
    match cell {
        Cell::Null => Ok(None),
        Cell::Integer(v) => Ok(Some(v.to_string())),
        Cell::Text(s) => Ok(Some(s.clone())),
        Cell::Vector(v) => {
            let joined = v
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Ok(Some(format!("[{joined}]")))
        }
        // float8 は PostgreSQL の float8 出力形式（Issue #1343・WIRE-13）。float4 公告列は
        // 呼び出し元 `cell_to_text_for_column` が先に `format_real` で処理する。
        Cell::Float(f) => Ok(Some(engine::scalar_float::format_double(*f))),
        Cell::Bool(b) => Ok(Some(if *b { "t".to_string() } else { "f".to_string() })),
        // `INTEGER`／`BIGINT` 列の投影結果（Issue #881・TABLE-13・TASK-196）。
        // `RowDescription` の OID 写像は `int4`／`int8`（Issue #903 レビュー指摘で
        // 是正。`column_wire_type` 参照）で、10進テキスト表現はどちらの OID とも
        // 一致するためここでは値そのものの変換のみ行う。
        Cell::SignedInteger(v) => Ok(Some(v.to_string())),
        // ISO テキストへ整形する（`engine::datetime` が単一情報源。TABLE-13・
        // TASK-197、Issue #884。RowDescription の OID は `date`（1082）／
        // `timestamp`（1114）を公告する。WIRE-13・TASK-200・Issue #895）。
        Cell::Date(days) => Ok(Some(engine::datetime::format_date(*days))),
        Cell::Timestamp(micros) => Ok(Some(engine::datetime::format_timestamp(*micros))),
        Cell::Array(array_value) => Ok(Some(pg_array_text(array_value))),
        // `BYTEA` のテキスト表現は PostgreSQL 既定の `bytea_output=hex`（`\x` ＋
        // 小文字 16 進）と同形にする（B5・Issue #886）。
        Cell::Bytes(b) => Ok(Some(engine::bytea::format_hex_text(b))),
        // `JSON`／`JSONB` の text 表現は格納テキストをそのまま出力する
        // （TABLE-14・Issue #889。`JSON` は入力テキスト保持・`JSONB` は正規化
        // 済みテキストのため、いずれもここで再シリアライズしない）。
        Cell::Json(s) => Ok(Some(s.clone())),
        // NUMERIC 列の text フォーマット表現は正規テキスト（`Decimal::Display`）
        // をそのまま送る（TABLE-13〔検討中〕・TASK-197、Issue #885）。
        Cell::Numeric(d) => Ok(Some(d.to_string())),
        // UUID 列の text フォーマット表現は正規テキスト（小文字
        // `8-4-4-4-12`）をそのまま送る（TABLE-13〔検討中〕・TASK-197、
        // Issue #887・U4）。
        Cell::Uuid(u) => Ok(Some(u.to_string())),
    }
}

/// 配列列（TABLE-14・TASK-198、Issue #888・D-A4、Issue #1193）を PostgreSQL 配列
/// テキスト形式（`{a,b}`）へ描画する。空配列は `{}`、NULL 要素は引用なしの `NULL`、
/// `BOOLEAN` 要素は `t`/`f`、整数は 10 進、`REAL`／`DOUBLE PRECISION` は
/// `engine::scalar_float` の整形（スカラー列と同じ表現）、`DATE`／`TIMESTAMP` は
/// `engine::datetime` の ISO 整形（`TIMESTAMP` は空白を含むため引用する）、`UUID` は
/// 正規テキスト。`TEXT` 要素のうち、空文字列・`,{}"\` や空白を含むもの・大小無視で
/// `NULL` に一致するものは `"..."` で囲み `"`・`\` をバックスラッシュでエスケープする
/// ため、文字列 `NULL` は NULL 要素と区別される（`RowDescription` の OID は他の
/// 非 VECTOR スカラー列と同じ text（25）のまま変更しない。WIRE-13）。
fn pg_array_text(array_value: &engine::row_codec::ArrayValue) -> String {
    use engine::row_codec::ArrayValue;

    fn render<T>(items: &[Option<T>], f: impl Fn(&T) -> String) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                Some(v) => f(v),
                None => "NULL".to_string(),
            })
            .collect()
    }

    let elements: Vec<String> = match array_value {
        ArrayValue::Text(items) => render(items, |s| quote_pg_array_text_elem(s)),
        ArrayValue::Bool(items) => render(items, |b| if *b { "t" } else { "f" }.to_string()),
        ArrayValue::Integer(items) => render(items, |v| v.to_string()),
        ArrayValue::BigInt(items) => render(items, |v| v.to_string()),
        ArrayValue::Real(items) => render(items, |v| engine::scalar_float::format_real(*v)),
        ArrayValue::Double(items) => render(items, |v| engine::scalar_float::format_double(*v)),
        ArrayValue::Date(items) => render(items, |v| engine::datetime::format_date(*v)),
        ArrayValue::Timestamp(items) => render(items, |v| {
            quote_pg_array_text_elem(&engine::datetime::format_timestamp(*v))
        }),
        ArrayValue::Uuid(items) => render(items, |v| v.to_string()),
        // Issue #1357: NUMERIC は正規テキスト（引用不要）、BYTEA は `\x` 16 進（`\` を
        // 含むため常に引用・エスケープされ PostgreSQL と同形の `"\\x0102"` になる）、
        // JSON／JSONB／ENUM は格納テキスト／ラベルを引用要否判定へ通す。出力は
        // `engine::sql::parser::parse_array_literal` で同じ値へ往復できる。
        ArrayValue::Numeric(items) => render(items, |d| d.to_string()),
        ArrayValue::Bytea(items) => render(items, |b| {
            quote_pg_array_text_elem(&engine::bytea::format_hex_text(b))
        }),
        ArrayValue::Json(items) | ArrayValue::Jsonb(items) => {
            render(items, |s| quote_pg_array_text_elem(s))
        }
        ArrayValue::Enum(items) => render(items, |s| quote_pg_array_text_elem(s)),
    };
    format!("{{{}}}", elements.join(","))
}

/// [`pg_array_text`] の TEXT 要素 1 個の引用要否判定・エスケープ処理。
fn quote_pg_array_text_elem(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `DataRow`（'D'）を `out` の末尾へ追記する（Issue #481。行ごとに新規
/// `Vec<u8>` を確保していた旧 [`encode_data_row`] を、呼び出し元
/// （`crate::response_buffer::ResponseBuffer` 経由の
/// `crate::simple_query::respond_query_result`）が持つ 1 個のバッファへ
/// 直接組み立てる形へ置き換えたもの。`row.cells` は呼び出し元の
/// `QueryResult::columns` と同じ順序・同じ長さであることを engine 側が保証する
/// （`sql::exec::execute_statement` の投影順。ポインタ: TASK-75・SQL-1〜4）。
///
/// 長さフィールド（フレーム先頭 4 バイト）は本体を書き終えるまで値が
/// 定まらないため、まずプレースホルダを push してから最後に
/// `out.get_mut` で backpatch する（`unwrap`/`[]` を使わない。取得できない
/// ことは有り得ないが、regression で `out` の構造が壊れた場合に panic では
/// なく `EncodeError` へ倒す）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （呼び出し元が完成済みフレームだけを送出できるようにするための契約。
/// 部分フレームを絶対に残さない）。
///
/// 全列テキスト形式（format code 0）の薄いラッパーで、
/// [`encode_data_row_body`] を「全列テキスト」の形式解決関数付きで呼ぶ
/// （既存呼び出し元・テストとの互換のため残す。生成バイト列は完全に同一。
/// WIRE-14・Issue #936）。
///
/// 呼び出しのたびに `formats` 用の `Vec<FormatCode>` を新規確保していた
/// 旧実装（[`encode_data_row_into_with_formats`] 経由）は、結果行数に
/// 比例したヒープ確保を発生させ「行ごとの `Vec<u8>` 確保を避ける」
/// （Issue #481）の最適化を損なっていたため、形式コードを列インデックスから
/// 直接解決する [`encode_data_row_body`] を共有する形へ変更した
/// （codex-review 指摘・PR #998）。
pub fn encode_data_row_into(
    columns: &[ColumnMeta],
    row: &ResultRow,
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    encode_data_row_body(columns, row, out, |_| ColumnEncoding::Text)
}

/// `DataRow`（'D'）を列ごとの [`FormatCode`] を反映して `out` の末尾へ追記する
/// （WIRE-14）。`formats.len() != row.cells.len()` は呼び出し元の内部不整合
/// として `EncodeError`（`XX000`）とする（untrusted 入力由来のバイナリ形式
/// 検査は [`validate_binary_formats`] が別途 `RowDescription` 送出前に
/// 済ませている前提）。
///
/// [`Null`](Cell::Null) セルは形式コードにかかわらず長さ `-1` を書く
/// （PostgreSQL の規約）。テキスト列のバイナリ表現は [`cell_to_text`] と
/// 同じ UTF-8 バイトをそのまま長さ付きで書く（[`binary::text`]）。
/// 列の型を知らないため、バイナリで受け付けるのは `Cell::Text` のみ
/// （他型の列は [`encode_data_row_into_with_columns`] を使う。Issue #1172）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （[`encode_data_row_into`] と同じ契約。部分フレームを絶対に残さない）。
pub fn encode_data_row_into_with_formats(
    columns: &[ColumnMeta],
    row: &ResultRow,
    formats: &[FormatCode],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    if row.cells.len() != formats.len() {
        return Err(EncodeError);
    }
    // `formats` は呼び出し元内部で長さ一致を確認済みだが、添字アクセス
    // （`[]`）は使わず `get` で明示的に処理する（coding-rust 規約）。
    encode_data_row_body(columns, row, out, |i| {
        match formats.get(i).copied().unwrap_or(FormatCode::Text) {
            FormatCode::Text => ColumnEncoding::Text,
            FormatCode::Binary => ColumnEncoding::Binary(None),
        }
    })
}

/// 列ごとの符号化方式（[`encode_data_row_body`] の内部表現）。
/// `Binary(None)` は列の型を知らない旧 API 用で `Cell::Text` だけを受け付ける。
#[derive(Clone, Copy)]
enum ColumnEncoding {
    Text,
    Binary(Option<WireType>),
}

/// `DataRow`（'D'）を列ごとの [`FormatCode`] と列メタ（[`ColumnMeta`]）を反映して
/// `out` の末尾へ追記する（WIRE-14・TASK-218・Issue #1172）。
///
/// wire-server の Execute（[`crate::extended_query`]）から呼ばれ、`INTEGER`／
/// `BIGINT`／`REAL`／`DOUBLE PRECISION`／`BOOLEAN`／`BYTEA`／`UUID`／`TEXT` の
/// バイナリ形式を PostgreSQL の send 形式で書く。`Cell` だけでは int4／int8・
/// float4／float8 を区別できないため列メタから型を解決する。
/// `columns`・`formats`・`row.cells` の長さが食い違う場合は `EncodeError`。
/// 型と `Cell` の不一致は text へフォールバックせず `EncodeError`（fail-closed）。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**。
pub fn encode_data_row_into_with_columns(
    row: &ResultRow,
    columns: &[ColumnMeta],
    formats: &[FormatCode],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    if row.cells.len() != formats.len() || row.cells.len() != columns.len() {
        return Err(EncodeError);
    }
    encode_data_row_body(columns, row, out, |i| {
        match formats.get(i).copied().unwrap_or(FormatCode::Text) {
            FormatCode::Text => ColumnEncoding::Text,
            FormatCode::Binary => ColumnEncoding::Binary(columns.get(i).map(column_wire_type)),
        }
    })
}

/// `bytes` を長さ付き（int4 長）で `out` へ追記する。
fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), EncodeError> {
    let len = i32::try_from(bytes.len()).map_err(|_| EncodeError)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 固定長型のバイナリ値を長さ付きで追記する。長さが `WireType::typlen()` と
/// 一致しない場合は内部不整合として `EncodeError`。
fn push_fixed(out: &mut Vec<u8>, wire_type: WireType, bytes: &[u8]) -> Result<(), EncodeError> {
    if i16::try_from(bytes.len()).ok() != Some(wire_type.typlen()) {
        return Err(EncodeError);
    }
    push_len_prefixed(out, bytes)
}

/// 非 NULL セル 1 個をバイナリ形式で `out` へ追記する（WIRE-14・Issue #1172）。
/// `wire_type` は列が公告する型。`None`（旧 API）は `Cell::Text` のみ受理する。
/// 型と `Cell` の不一致・範囲外・無損失でない float4 縮小は `EncodeError`
/// （text へ黙って落とさない）。
fn encode_binary_cell(
    wire_type: Option<WireType>,
    cell: &Cell,
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let wire_type = match wire_type {
        Some(t) => t,
        None => {
            return match cell {
                Cell::Text(s) => push_len_prefixed(out, binary::text(s)),
                _ => Err(EncodeError),
            };
        }
    };
    match (wire_type, cell) {
        (WireType::Text, Cell::Text(s)) => push_len_prefixed(out, binary::text(s)),
        (WireType::Int4, Cell::SignedInteger(v)) => {
            let v = i32::try_from(*v).map_err(|_| EncodeError)?;
            push_fixed(out, wire_type, &binary::int4(v))
        }
        (WireType::Int8, Cell::SignedInteger(v)) => push_fixed(out, wire_type, &binary::int8(*v)),
        // `COUNT`・`ROW_NUMBER`/`RANK` 系は `int8` を公告しつつ `Cell::Integer(u64)` を返す
        // （Issue #1407）。`i64` に収まらない値は fail-closed（`XX000`）。
        (WireType::Int8, Cell::Integer(v)) => {
            let v = i64::try_from(*v).map_err(|_| EncodeError)?;
            push_fixed(out, wire_type, &binary::int8(v))
        }
        (WireType::Float4, Cell::Float(v)) => {
            let narrowed = if v.is_nan() { f32::NAN } else { *v as f32 };
            // NaN 以外は f64 へ戻して一致する（無損失）場合のみ受理する。
            if !v.is_nan() && f64::from(narrowed) != *v {
                return Err(EncodeError);
            }
            push_fixed(out, wire_type, &binary::float4(narrowed))
        }
        (WireType::Float8, Cell::Float(v)) => push_fixed(out, wire_type, &binary::float8(*v)),
        (WireType::Bool, Cell::Bool(b)) => push_fixed(out, wire_type, &binary::bool_(*b)),
        (WireType::Bytea, Cell::Bytes(b)) => push_len_prefixed(out, binary::bytea(b)),
        (WireType::Uuid, Cell::Uuid(u)) => push_fixed(out, wire_type, &binary::uuid(*u.as_bytes())),
        // 型と Cell の不一致、および非対応型（事前検査
        // `validate_binary_formats` で拒否済みの多層防御）は fail-closed。
        _ => Err(EncodeError),
    }
}

/// [`encode_data_row_into`]／[`encode_data_row_into_with_formats`]／
/// [`encode_data_row_into_with_columns`] が共有するフレーム組み立て本体。
/// `encoding_at(i)` は列インデックス `i` の符号化方式を返す。
///
/// **失敗時は `out` を呼び出し前の長さへ必ず `truncate` してから返す**
/// （部分フレームを絶対に残さない）。
fn encode_data_row_body<F>(
    columns: &[ColumnMeta],
    row: &ResultRow,
    out: &mut Vec<u8>,
    encoding_at: F,
) -> Result<(), EncodeError>
where
    F: Fn(usize) -> ColumnEncoding,
{
    // 列メタ（値の text 整形が列の公告型に依存する。Issue #1173）と行のセル数の
    // 不一致は呼び出し元の内部不整合。何も書く前に fail-closed に拒否する。
    if columns.len() != row.cells.len() {
        return Err(EncodeError);
    }
    let start = out.len();
    let field_count = match i16::try_from(row.cells.len()) {
        Ok(n) => n,
        Err(_) => {
            out.truncate(start);
            return Err(EncodeError);
        }
    };

    let mut write_body = || -> Result<(), EncodeError> {
        out.push(b'D');
        // 長さフィールドのプレースホルダ（後で backpatch）。
        out.extend_from_slice(&0i32.to_be_bytes());
        let body_start = out.len();
        out.extend_from_slice(&field_count.to_be_bytes());
        for (i, cell) in row.cells.iter().enumerate() {
            match encoding_at(i) {
                ColumnEncoding::Text => {
                    match cell_to_text_for_column(columns.get(i).ok_or(EncodeError)?, cell)? {
                        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
                        Some(text) => push_len_prefixed(out, text.as_bytes())?,
                    }
                }
                ColumnEncoding::Binary(wire_type) => match cell {
                    Cell::Null => out.extend_from_slice(&(-1i32).to_be_bytes()),
                    other => encode_binary_cell(wire_type, other, out)?,
                },
            }
        }
        let body_len = out.len().checked_sub(body_start).ok_or(EncodeError)?;
        let total_len = frame_len(body_len)?;
        let len_pos = body_start.checked_sub(4).ok_or(EncodeError)?;
        let len_slice = out
            .get_mut(len_pos..len_pos.checked_add(4).ok_or(EncodeError)?)
            .ok_or(EncodeError)?;
        len_slice.copy_from_slice(&total_len.to_be_bytes());
        Ok(())
    };

    match write_body() {
        Ok(()) => Ok(()),
        Err(e) => {
            out.truncate(start);
            Err(e)
        }
    }
}
/// `DataRow`（'D'）を 1 行ぶん新規 `Vec<u8>` として組み立てる。
/// [`encode_data_row_into`] を呼ぶ薄いラッパーで、既存呼び出し元・テストとの
/// 互換のため残す（生成バイト列は完全に同一）。
pub fn encode_data_row(columns: &[ColumnMeta], row: &ResultRow) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    encode_data_row_into(columns, row, &mut out)?;
    Ok(out)
}

/// `CommandComplete`（'C'）。`tag` は `SELECT n` / `INSERT 0 1` / `SET` /
/// `CREATE FUNCTION` 等、簡易クエリプロトコルが規定するコマンドタグ文字列。
pub fn encode_command_complete(tag: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::with_capacity(tag.len() + 1);
    body.extend_from_slice(tag.as_bytes());
    body.push(0);
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'C');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `ReadyForQuery`（'Z'）。固定長 6 バイト（タグ 1 + 長さ 4 + トランザクション
/// 状態 1）。`crate::handshake::write_ready_for_query` から使う唯一のレイアウト
/// 実体（Issue #481。以前は同モジュール内にバイト列組み立てが個別に存在し、
/// `crate::response_buffer::ResponseBuffer` へ他フレームと同じ形で積める
/// フレームが無かった）。状態バイトは `status`（[`engine::sql::transaction::
/// SessionTransaction::status`] が返す接続単位の明示トランザクション状態。
/// SQL-31・TASK-221・WIRE-19）をそのまま `'I'`（idle）／`'T'`（in transaction）／
/// `'E'`（failed transaction）へ写像する。呼び出し元が明示トランザクションを
/// 持たない箇所（ハンドシェイク直後・COPY サブプロトコル。COPY は明示
/// トランザクション中には到達しない）は `TransactionStatus::Idle` を渡す。
pub fn encode_ready_for_query(status: engine::sql::transaction::TransactionStatus) -> [u8; 6] {
    use engine::sql::transaction::TransactionStatus;
    let mut msg = [0u8; 6];
    msg[0] = b'Z';
    let len_bytes = 5i32.to_be_bytes();
    msg[1] = len_bytes[0];
    msg[2] = len_bytes[1];
    msg[3] = len_bytes[2];
    msg[4] = len_bytes[3];
    msg[5] = match status {
        TransactionStatus::Idle => b'I',
        TransactionStatus::InTransaction => b'T',
        TransactionStatus::Failed => b'E',
    };
    msg
}

/// `EmptyQueryResponse`（'I'）。body なし・長さ固定（4）。
pub fn encode_empty_query_response() -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(b'I');
    msg.extend_from_slice(&4i32.to_be_bytes());
    msg
}

/// `ParseComplete`（'1'）。拡張クエリプロトコルの Parse（Issue #933・TASK-71・
/// WIRE-11）が成功したことを示す固定応答。body なし・長さ固定（4）。
pub fn encode_parse_complete() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'1';
    let len_bytes = 4i32.to_be_bytes();
    msg[1..5].copy_from_slice(&len_bytes);
    msg
}

/// `ParameterDescription`（'t'）。Describe（'D' 種別 S。Issue #933）が返す
/// パラメータ型 OID 一覧。`$n` 束縛（WIRE-12・#935）は本 Issue の対象外のため
/// `param_oids` は常に空スライスで呼ばれる契約だが、将来の非空呼び出しにも
/// 対応できる汎用実装としておく。件数は `i16` に収まる必要がある。
pub fn encode_parameter_description(param_oids: &[i32]) -> Result<Vec<u8>, EncodeError> {
    let count = i16::try_from(param_oids.len()).map_err(|_| EncodeError)?;
    let mut body = Vec::with_capacity(2 + param_oids.len() * 4);
    body.extend_from_slice(&count.to_be_bytes());
    for oid in param_oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b't');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `NoData`（'n'）。Describe（'D' 種別 S。Issue #933）の対象文が結果列を持たない
/// （`RETURNING` の無い DML・`SET`・`CREATE FUNCTION`・`TRUNCATE`）ことを示す
/// 固定応答。body なし・長さ固定（4）。
pub fn encode_no_data() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'n';
    let len_bytes = 4i32.to_be_bytes();
    msg[1..5].copy_from_slice(&len_bytes);
    msg
}

/// `BindComplete`（'2'）。拡張クエリプロトコルの Bind（Issue #934・TASK-71・
/// WIRE-11）が成功したことを示す固定応答。body なし・長さ固定（4）。
pub fn encode_bind_complete() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'2';
    msg[1..5].copy_from_slice(&4i32.to_be_bytes());
    msg
}

/// `CloseComplete`（'3'）。拡張クエリプロトコルの Close（Issue #934）が
/// statement／portal いずれかを解放したことを示す固定応答（対象が未存在でも
/// 同じ応答を返す。PostgreSQL と同じ挙動）。body なし・長さ固定（4）。
pub fn encode_close_complete() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'3';
    msg[1..5].copy_from_slice(&4i32.to_be_bytes());
    msg
}

/// `PortalSuspended`（'s'）。拡張クエリプロトコルの Execute（Issue #934）が
/// `max_rows` 制限により行の送出を打ち切り、続きを次の Execute へ持ち越す
/// ことを示す固定応答。body なし・長さ固定（4）。
pub fn encode_portal_suspended() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b's';
    msg[1..5].copy_from_slice(&4i32.to_be_bytes());
    msg
}

/// `ErrorResponse`（'E'）body の `S`（severity）/`C`（sqlstate）/`M`（message）の
/// 3 フィールドを `body` へ書き込む。フィールド終端（末尾の NUL）・フレーム化
/// （`E` タグ・長さ）は呼び出し元が行う。
///
/// `severity` は呼び出し元が明示的に決定した値をそのまま書き込む（本関数自身は
/// `ErrorClass` 等から severity を判定しない）。接続を閉じる契約の分類
/// （`ErrorClass::ConnectionLimitExceeded`＝`53300`）は `FATAL`、それ以外は
/// `ERROR` を渡す契約（codex-review P1 指摘対応・PR #258。`crate::error_response`
/// の呼び出し元が `severity_for` で決定し本関数へ渡す。写像を本関数側で持たせると
/// `limits.rs::reject_too_many_connections` の独自実装〔`FATAL` 固定〕と分類の
/// 判定基準が 2 箇所に分散するため、判定は呼び出し元に閉じる）。
///
/// [`encode_error_response`] と `crate::error_response::encode`（TASK-153・ERR-1。
/// `ErrorClass` から severity/SQLSTATE を一元的に決定する横断写像）が共有する
/// 唯一のレイアウト実体（codex-review Low 指摘対応・PR #101。以前は両者が同じ
/// S/C/M 書き込みをそれぞれ自前で複製しており、フィールドレイアウト変更時に
/// 乖離しうる状態だった）。crate 内に限り公開する。
pub(crate) fn push_s_c_m_fields(body: &mut Vec<u8>, severity: &str, sqlstate: &str, message: &str) {
    body.push(b'S');
    body.extend_from_slice(severity.as_bytes());
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(sqlstate.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
}

/// `ErrorResponse`（'E'）の `D`（detail）フィールドを 1 個 `body` へ追記する
/// （タグ 1 バイト＋値＋NUL 終端。[`push_s_c_m_fields`] と同じ「フィールド
/// 終端・フレーム化は呼び出し元が担う」方針）。
///
/// 呼び出し元は `crate::error_response::encode_with_detail` のみ（ERR-5・
/// TASK-153 ポインタ。`RECOVER-5` (3) の commit 後 panic 緊急応答で
/// `state=may_be_committed` を搬送する用途）。`crate` 内限定公開とし、
/// wire-server 外へは公開しない。
pub(crate) fn push_d_field(body: &mut Vec<u8>, detail: &str) {
    body.push(b'D');
    body.extend_from_slice(detail.as_bytes());
    body.push(0);
}

/// `ErrorResponse`（'E'）を `S`/`C`/`M` の 3 フィールドのみ、severity `ERROR`
/// 固定で組み立てる（`sqlstate`/`message`。他テナント・存在情報は含めない）。
///
/// **本 crate の通常・緊急いずれの送出経路もこの関数は経由しない**
/// （TASK-153・ERR-1・codex-review P1 指摘対応・PR #258）。`handshake.rs::
/// write_error_response`・`crate::simple_query::build_emergency_response_bytes`
/// はいずれも `crate::error_response::encode`（`ErrorClass` から severity・
/// SQLSTATE を一元的に決定する横断写像。severity は分類ごとに変わりうるため、
/// 本関数の `ERROR` 固定では `ErrorClass::ConnectionLimitExceeded` の `FATAL`
/// 契約を表現できない）を使う。フィールド書き込みの実体
/// （[`push_s_c_m_fields`]）は共有するため、`sqlstate`/`message` を直接受け取る
/// 薄い公開 API として残す（severity を常に `ERROR` に固定できる用途向け）。
///
/// フレーム長の算出は既存 encoder（[`encode_command_complete`] 等）と同じ
/// `checked` 方式（[`frame_len`]）を使い、`as i32` によるオーバーフローを
/// 起こさない（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」。
/// 本関数の入力自体は untrusted ではないが、同じ規律を踏襲する）。
pub fn encode_error_response(sqlstate: &str, message: &str) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::new();
    push_s_c_m_fields(&mut body, "ERROR", sqlstate, message);
    body.push(0); // フィールド終端
    let total_len = frame_len(body.len())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行のセル種別から対応する列メタを組み立てる（`encode_data_row*` が列メタを
    /// 要求するようになった Issue #1173 以降、値の整形が列型に依存しない既存
    /// テスト向けの薄いアダプタ。`Float` は `DOUBLE PRECISION` 列として扱う）。
    fn columns_for(row: &ResultRow) -> Vec<ColumnMeta> {
        row.cells
            .iter()
            .enumerate()
            .map(|(i, cell)| ColumnMeta::Scalar {
                name: format!("c{i}"),
                ty: match cell {
                    Cell::Float(_) => engine::catalog::ColumnType::Double,
                    _ => engine::catalog::ColumnType::Text,
                },
            })
            .collect()
    }

    fn encode_data_row(row: &ResultRow) -> Result<Vec<u8>, EncodeError> {
        super::encode_data_row(&columns_for(row), row)
    }

    fn encode_data_row_into(row: &ResultRow, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        super::encode_data_row_into(&columns_for(row), row, out)
    }

    fn encode_data_row_into_with_formats(
        row: &ResultRow,
        formats: &[FormatCode],
        out: &mut Vec<u8>,
    ) -> Result<(), EncodeError> {
        super::encode_data_row_into_with_formats(&columns_for(row), row, formats, out)
    }

    /// Issue #1173: `Computed{ty}` は `Scalar{ty}` と同じ OID・typlen を公告し、
    /// `ty: None` は text にフォールバックする。
    #[test]
    fn computed_column_wire_type_follows_static_type() {
        use engine::catalog::ColumnType;
        let computed = |ty: Option<ColumnType>| ColumnMeta::Computed {
            name: "x".to_string(),
            ty,
        };
        let cases: Vec<(Option<ColumnType>, i32, i16)> = vec![
            (Some(ColumnType::BigInt), 20, 8),
            (Some(ColumnType::Integer), 23, 4),
            (Some(ColumnType::Real), 700, 4),
            (Some(ColumnType::Double), 701, 8),
            (Some(ColumnType::Boolean), 16, 1),
            (Some(ColumnType::Text), 25, -1),
            (Some(ColumnType::Date), 1082, 4),
            (Some(ColumnType::Timestamp), 1114, 8),
            (
                Some(ColumnType::Numeric {
                    precision: 38,
                    scale: 2,
                }),
                1700,
                -1,
            ),
            (None, 25, -1),
        ];
        for (ty, oid, typlen) in cases {
            let wire_type = column_wire_type(&computed(ty.clone()));
            assert_eq!(wire_type.oid(), oid, "oid for {ty:?}");
            assert_eq!(wire_type.typlen(), typlen, "typlen for {ty:?}");
        }
        // Computed 列は静的型が対応型ならバイナリ可、非対応型・`ty: None` は
        // fail-closed のまま（Issue #1407）。
        assert!(column_binary_support(&computed(Some(ColumnType::BigInt))));
        assert!(!column_binary_support(&computed(None)));
        assert!(!column_binary_support(&computed(Some(
            ColumnType::Numeric {
                precision: 38,
                scale: 2,
            }
        ))));
    }

    fn real_column() -> ColumnMeta {
        ColumnMeta::Computed {
            name: "r".to_string(),
            ty: Some(engine::catalog::ColumnType::Real),
        }
    }

    /// Issue #1173: `float4` 列の `Cell::Float` は PostgreSQL の float4 出力形式で
    /// 出る。`float8` 列は従来どおり f64 の `Display`。
    #[test]
    fn float4_column_text_uses_postgresql_float4_format() {
        let f4 = |v: f32| Cell::Float(f64::from(v));
        assert_eq!(
            cell_to_text_for_column(&real_column(), &f4(0.1)).unwrap(),
            Some("0.1".to_string())
        );
        assert_eq!(
            cell_to_text_for_column(&real_column(), &f4(1e6)).unwrap(),
            Some("1e+06".to_string())
        );
        let double = ColumnMeta::Scalar {
            name: "d".to_string(),
            ty: engine::catalog::ColumnType::Double,
        };
        assert_eq!(
            cell_to_text_for_column(&double, &Cell::Float(f64::from(0.1f32))).unwrap(),
            Some("0.10000000149011612".to_string())
        );
        assert_eq!(
            cell_to_text_for_column(&real_column(), &Cell::Null).unwrap(),
            None
        );
    }

    /// f32 で表現できない f64 が `float4` 列に来たら fail-closed（`EncodeError`）。
    #[test]
    fn float4_column_rejects_value_not_representable_as_f32() {
        assert!(cell_to_text_for_column(&real_column(), &Cell::Float(0.1f64)).is_err());
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Float(0.1f64)],
        };
        let mut out = vec![b'x'];
        assert!(super::encode_data_row_into(&[real_column()], &row, &mut out).is_err());
        assert_eq!(
            out,
            vec![b'x'],
            "failed encode must not leave partial frame"
        );
    }

    /// 列メタ数とセル数の不一致は `EncodeError`（内部不整合の fail-closed）。
    #[test]
    fn data_row_rejects_column_count_mismatch() {
        let row = ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Bool(true), Cell::Bool(false)],
        };
        assert!(super::encode_data_row(&[real_column()], &row).is_err());
        assert!(super::encode_data_row(&[], &row).is_err());
    }

    /// テストのみが使うバイト列アサーションヘルパー（`.claude/rules/
    /// coding-rust.md` の添字アクセス禁止は untrusted 受信入力経路が対象だが、
    /// `[]` の使用箇所をレビュー時に P0 判定と混同されないよう `get()` で統一する）。
    fn byte_at(msg: &[u8], idx: usize) -> u8 {
        *msg.get(idx).expect("message too short")
    }

    fn i32_at(msg: &[u8], idx: usize) -> i32 {
        let bytes: [u8; 4] = msg
            .get(idx..idx + 4)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 4 bytes");
        i32::from_be_bytes(bytes)
    }

    fn i16_at(msg: &[u8], idx: usize) -> i16 {
        let bytes: [u8; 2] = msg
            .get(idx..idx + 2)
            .expect("message too short")
            .try_into()
            .expect("slice is exactly 2 bytes");
        i16::from_be_bytes(bytes)
    }

    fn slice_at(msg: &[u8], idx: usize, len: usize) -> &[u8] {
        msg.get(idx..idx + len).expect("message too short")
    }

    #[test]
    fn row_description_encodes_id_and_text_columns() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ];
        let msg = encode_row_description(&columns).expect("encode");
        assert_eq!(byte_at(&msg, 0), b'T');
        // フィールド数は body 先頭の i16（type/len/OID ヘッダ直後の先頭 4 バイトが
        // 'T' + length のため、body はインデックス 5 から始まる）。
        assert_eq!(i16_at(&msg, 5), 2);
    }

    #[test]
    fn data_row_null_cell_has_length_negative_one() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 'D' + length(4) + field_count(2) + cell length(4)
        assert_eq!(i32_at(&msg, 7), -1);
    }

    #[test]
    fn data_row_vector_cell_is_bracketed_text() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Vector(vec![1.0, 2.5])],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "[1,2.5]");
    }

    #[test]
    fn data_row_bool_cell_encodes_t_or_f() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true), Cell::Bool(false)],
        };
        let msg = encode_data_row(&row).expect("encode");
        // 先頭 field: 'D' + length(4) + field_count(2) = idx 7 から length(4) + 't'
        let first_len = i32_at(&msg, 7) as usize;
        assert_eq!(first_len, 1);
        assert_eq!(slice_at(&msg, 11, first_len), b"t");
    }

    #[test]
    fn row_description_encodes_integer_and_bigint_columns_as_int4_int8() {
        // Issue #903 レビュー指摘: `INTEGER`／`BIGINT` 列を一律 `text`（OID 25）
        // で公告すると psql・ドライバ・ORM が整数列を文字列として扱ってしまう
        // ため、`int4`（OID 23）／`int8`（OID 20）へ正しく写像することを固定する。
        let columns = vec![
            ColumnMeta::Scalar {
                name: "n".to_string(),
                ty: engine::catalog::ColumnType::Integer,
            },
            ColumnMeta::Scalar {
                name: "b".to_string(),
                ty: engine::catalog::ColumnType::BigInt,
            },
        ];
        assert_eq!(column_wire_type(&columns[0]), WireType::Int4);
        assert_eq!(column_wire_type(&columns[1]), WireType::Int8);
        assert_eq!(WireType::Int4.oid(), 23);
        assert_eq!(WireType::Int8.oid(), 20);
        assert_eq!(WireType::Int4.typlen(), 4);
        assert_eq!(WireType::Int8.typlen(), 8);
        assert_eq!(WireType::Int4.pg_type_name(), "int4");
        assert_eq!(WireType::Int8.pg_type_name(), "int8");

        let msg = encode_row_description(&columns).expect("encode");
        // body はインデックス 5 から始まる（'T' + length(4)）。
        assert_eq!(i16_at(&msg, 5), 2);
    }

    #[test]
    fn data_row_array_text_cell_encodes_pg_array_text_with_quoting() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Text(vec![
                Some("a".to_string()),
                Some("b c".to_string()),
                Some("".to_string()),
                Some("d\"e".to_string()),
                Some("NULL".to_string()),
                None,
            ]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        // 文字列 `NULL` は引用され、NULL 要素（引用なし `NULL`）と区別される。
        assert_eq!(text, r#"{a,"b c","","d\"e","NULL",NULL}"#);
    }

    /// Issue #1193: 新しい要素型と NULL 要素の text 表現。
    #[test]
    fn data_row_array_new_element_types_encode_with_null_elements() {
        use engine::row_codec::ArrayValue;
        let text_of = |value: ArrayValue| {
            let row = ResultRow {
                id: 1,
                score: 0.0,
                cells: vec![Cell::Array(value)],
            };
            let msg = encode_data_row(&row).expect("encode");
            let cell_len = i32_at(&msg, 7) as usize;
            std::str::from_utf8(slice_at(&msg, 11, cell_len))
                .expect("utf8")
                .to_string()
        };
        assert_eq!(
            text_of(ArrayValue::Integer(vec![Some(1), None, Some(-3)])),
            "{1,NULL,-3}"
        );
        assert_eq!(
            text_of(ArrayValue::BigInt(vec![Some(9_007_199_254_740_993), None])),
            "{9007199254740993,NULL}"
        );
        assert_eq!(
            text_of(ArrayValue::Real(vec![Some(1.5), None])),
            "{1.5,NULL}"
        );
        assert_eq!(
            text_of(ArrayValue::Double(vec![Some(2.25), Some(0.1)])),
            "{2.25,0.1}"
        );
        assert_eq!(
            text_of(ArrayValue::Date(vec![Some(1), None])),
            "{1970-01-02,NULL}"
        );
        assert_eq!(
            text_of(ArrayValue::Timestamp(vec![Some(1_000_000), None])),
            r#"{"1970-01-01 00:00:01",NULL}"#
        );
        assert_eq!(
            text_of(ArrayValue::Uuid(vec![
                Some(engine::uuid::Uuid::from_bytes([0x11; 16])),
                None
            ])),
            "{11111111-1111-1111-1111-111111111111,NULL}"
        );
        assert_eq!(
            text_of(ArrayValue::Bool(vec![None, Some(true)])),
            "{NULL,t}"
        );
    }

    #[test]
    fn data_row_array_bool_cell_encodes_t_f() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Bool(vec![Some(true), Some(false)]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "{t,f}");
    }

    #[test]
    fn data_row_array_empty_cell_encodes_braces_only() {
        use engine::row_codec::ArrayValue;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Array(ArrayValue::Text(vec![]))],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, "{}");
    }

    #[test]
    fn data_row_integer_cell_within_i64_range_encodes_as_decimal() {
        let row = ResultRow {
            id: i64::MAX as u64,
            score: 0.0,
            cells: vec![Cell::Integer(i64::MAX as u64)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, i64::MAX.to_string());
    }

    #[test]
    fn data_row_integer_cell_beyond_i64_max_encodes_as_decimal() {
        // `id` 列は `numeric`（OID 1700）として公告するため（型写像表参照）、
        // `i64::MAX` を超える正当な `u64` 行 ID も値域制限なくそのまま10進
        // テキストで送出できる（PR #210 レビュー指摘: 旧実装は int8 前提で
        // `EncodeError`/`XX000` にしていた）。
        let row = ResultRow {
            id: u64::MAX,
            score: 0.0,
            cells: vec![Cell::Integer(u64::MAX)],
        };
        let msg = encode_data_row(&row).expect("encode");
        let cell_len = i32_at(&msg, 7) as usize;
        let text = std::str::from_utf8(slice_at(&msg, 11, cell_len)).expect("utf8");
        assert_eq!(text, u64::MAX.to_string());
    }

    // --- encode_data_row_into（Issue #481）---

    #[test]
    fn encode_data_row_into_matches_encode_data_row_byte_for_byte() {
        let row = ResultRow {
            id: 7,
            score: 0.0,
            cells: vec![Cell::Integer(7), Cell::Text("lang".to_string()), Cell::Null],
        };
        let standalone = encode_data_row(&row).expect("encode standalone");
        let mut out = Vec::new();
        encode_data_row_into(&row, &mut out).expect("encode into");
        assert_eq!(out, standalone);
    }

    #[test]
    fn encode_data_row_into_appends_after_existing_content() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true)],
        };
        let mut out = b"PREFIX".to_vec();
        encode_data_row_into(&row, &mut out).expect("encode into");
        assert!(out.starts_with(b"PREFIX"));
        assert_eq!(byte_at(&out, 6), b'D');
    }

    #[test]
    fn encode_data_row_into_truncates_back_to_start_on_failure() {
        // field_count は i16 に収まる必要がある（32,768 セルは超過）。
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null; 32_768],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into(&row, &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "failed encode must not leave partial bytes");
    }

    #[test]
    fn ready_for_query_has_fixed_layout() {
        let msg = encode_ready_for_query(engine::sql::transaction::TransactionStatus::Idle);
        assert_eq!(msg, [b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn ready_for_query_reflects_in_transaction_status() {
        let msg =
            encode_ready_for_query(engine::sql::transaction::TransactionStatus::InTransaction);
        assert_eq!(msg, [b'Z', 0, 0, 0, 5, b'T']);
    }

    #[test]
    fn ready_for_query_reflects_failed_status() {
        let msg = encode_ready_for_query(engine::sql::transaction::TransactionStatus::Failed);
        assert_eq!(msg, [b'Z', 0, 0, 0, 5, b'E']);
    }

    #[test]
    fn command_complete_contains_tag() {
        let msg = encode_command_complete("SELECT 3").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'C');
        assert!(msg.ends_with(b"SELECT 3\0"));
    }

    #[test]
    fn empty_query_response_has_fixed_length_and_no_body() {
        let msg = encode_empty_query_response();
        assert_eq!(msg, vec![b'I', 0, 0, 0, 4]);
    }

    // --- encode_error_response（TASK-97・RECOVER-6、codex-review P1・PR #253 指摘対応）---

    #[test]
    fn error_response_contains_s_c_m_fields_and_terminator_only() {
        let msg = encode_error_response("XX000", "internal error").expect("encode");
        assert_eq!(byte_at(&msg, 0), b'E');

        // body は 'E' + length(4 バイト) の直後（インデックス 5）から始まる。
        let body = slice_at(&msg, 5, msg.len() - 5);
        assert_eq!(body.first().copied(), Some(b'S'));
        assert!(body.windows(6).any(|w| w == b"ERROR\0"));
        assert!(body.windows(6).any(|w| w == b"XX000\0"));
        assert!(body.windows(15).any(|w| w == b"internal error\0"));
        // 通常応答（`encode_error_response`／`crate::error_response::encode`）は
        // `D` を付けない契約（ERR-5。緊急応答チャネルのみ `crate::error_response::
        // encode_with_detail` 経由で `D` を付与する。`crate::error_response`
        // モジュールドキュメント参照）。
        assert!(!body.contains(&b'D'));
        assert_eq!(
            body.last().copied(),
            Some(0),
            "field terminator (double NUL)"
        );

        // 長さフィールドは i32 で self を含む total_len。
        let declared_len = i32_at(&msg, 1) as usize;
        assert_eq!(
            declared_len,
            msg.len() - 1,
            "length field excludes only the leading 'E' type byte"
        );
    }

    // --- ResultFormats::resolve（WIRE-14・Issue #936）---

    #[test]
    fn result_formats_resolve_zero_codes_defaults_all_text() {
        let formats = ResultFormats::new(&[]).resolve(3).expect("resolve");
        assert_eq!(formats, vec![FormatCode::Text; 3]);
    }

    #[test]
    fn result_formats_resolve_single_code_applies_to_all_columns() {
        let formats = ResultFormats::new(&[1]).resolve(3).expect("resolve");
        assert_eq!(formats, vec![FormatCode::Binary; 3]);
    }

    #[test]
    fn result_formats_resolve_per_column_codes() {
        let formats = ResultFormats::new(&[0, 1, 0]).resolve(3).expect("resolve");
        assert_eq!(
            formats,
            vec![FormatCode::Text, FormatCode::Binary, FormatCode::Text]
        );
    }

    #[test]
    fn result_formats_resolve_count_mismatch_is_rejected() {
        let err = ResultFormats::new(&[0, 1]).resolve(3).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn result_formats_resolve_invalid_value_is_rejected() {
        let err = ResultFormats::new(&[2]).resolve(1).unwrap_err();
        assert_eq!(err, BinaryFormatError::InvalidFormatCode);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    // --- validate_binary_formats（WIRE-14）---

    #[test]
    fn validate_binary_formats_accepts_text_column_as_binary() {
        let columns = vec![ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }];
        let formats = vec![FormatCode::Binary];
        assert!(validate_binary_formats(&columns, &formats).is_ok());
    }

    #[test]
    fn validate_binary_formats_rejects_id_column_as_binary() {
        let columns = vec![ColumnMeta::Id];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_vector_column_as_binary() {
        let columns = vec![ColumnMeta::Scalar {
            name: "embedding".to_string(),
            ty: engine::catalog::ColumnType::Vector(3),
        }];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_computed_column_as_binary() {
        let columns = vec![ColumnMeta::Computed {
            name: "expr".to_string(),
            ty: None,
        }];
        let formats = vec![FormatCode::Binary];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class(), ErrorClass::FeatureNotSupported);
    }

    #[test]
    fn validate_binary_formats_rejects_formats_shorter_than_columns() {
        // `.zip()` の共通部分のみ検査すると formats が短い（列数不足）場合に
        // 素通りしてしまう（codex-review 指摘・Issue #936 PR #998）。
        // 事前検査という契約どおり `08P01` で拒否する。
        let columns = vec![
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
        ];
        let formats = vec![FormatCode::Text];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn validate_binary_formats_rejects_formats_longer_than_columns() {
        // formats が列数より多い場合も同様に `08P01` で拒否する
        // （`.zip()` は左側 `columns` の長さで打ち切るため長すぎる側も
        // 素通りしていた）。
        let columns = vec![ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }];
        let formats = vec![FormatCode::Text, FormatCode::Text];
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::FormatCountMismatch);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
    }

    #[test]
    fn validate_binary_formats_accepts_all_text_columns_unconditionally() {
        // バイナリを一切要求しない場合は非対応型が混在していても通る
        // （検査対象は「バイナリを要求された列」のみ）。
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: engine::catalog::ColumnType::Vector(3),
            },
            ColumnMeta::Computed {
                name: "expr".to_string(),
                ty: None,
            },
        ];
        let formats = vec![FormatCode::Text; 3];
        assert!(validate_binary_formats(&columns, &formats).is_ok());
    }

    // --- binary モジュール（golden バイト列。WIRE-14）---

    #[test]
    fn binary_int4_golden_bytes() {
        assert_eq!(binary::int4(1), [0x00, 0x00, 0x00, 0x01]);
        assert_eq!(binary::int4(-1), [0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn binary_int8_golden_bytes() {
        assert_eq!(
            binary::int8(-1),
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            binary::int8(1),
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn binary_float8_golden_bytes() {
        // IEEE 754 倍精度で 1.0 は指数部 1023（バイアス済み）・仮数部 0。
        assert_eq!(
            binary::float8(1.0),
            [0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn binary_float4_golden_bytes() {
        assert_eq!(binary::float4(1.0), [0x3f, 0x80, 0x00, 0x00]);
    }

    #[test]
    fn binary_bool_golden_bytes() {
        assert_eq!(binary::bool_(true), [0x01]);
        assert_eq!(binary::bool_(false), [0x00]);
    }

    #[test]
    fn binary_uuid_round_trips() {
        let uuid_bytes: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        assert_eq!(binary::uuid(uuid_bytes), uuid_bytes);
    }

    #[test]
    fn binary_text_is_utf8_bytes() {
        // マルチバイト文字（'é' は UTF-8 で 2 バイト）でも生バイトをそのまま返す。
        assert_eq!(binary::text("é"), "é".as_bytes());
        assert_eq!(binary::text("é").len(), 2);
    }

    #[test]
    fn binary_bytea_is_identity() {
        let data = [1u8, 2, 3];
        assert_eq!(binary::bytea(&data), &data);
    }

    // --- encode_row_description_with_formats / encode_data_row_into_with_formats
    // の全テキスト不変性（受け入れ条件 4・WIRE-14）---

    #[test]
    fn row_description_with_all_text_formats_matches_legacy_encoder() {
        let columns = vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: engine::catalog::ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: engine::catalog::ColumnType::Vector(3),
            },
            ColumnMeta::Computed {
                name: "expr".to_string(),
                ty: None,
            },
        ];
        let legacy = encode_row_description(&columns).expect("legacy encode");
        let formats = vec![FormatCode::Text; columns.len()];
        let via_formats =
            encode_row_description_with_formats(&columns, &formats).expect("formats encode");
        assert_eq!(legacy, via_formats);
    }

    #[test]
    fn data_row_with_all_text_formats_matches_legacy_encoder() {
        let row = ResultRow {
            id: 7,
            score: 0.0,
            cells: vec![
                Cell::Integer(7),
                Cell::Text("lang".to_string()),
                Cell::Null,
                Cell::Vector(vec![1.0, 2.5]),
                Cell::Bool(true),
                Cell::Float(1.5),
            ],
        };
        let legacy = encode_data_row(&row).expect("legacy encode");
        let formats = vec![FormatCode::Text; row.cells.len()];
        let mut via_formats = Vec::new();
        encode_data_row_into_with_formats(&row, &formats, &mut via_formats)
            .expect("formats encode");
        assert_eq!(legacy, via_formats);
    }

    #[test]
    fn data_row_into_with_formats_rejects_count_mismatch() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Integer(1)],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into_with_formats(&row, &[], &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "mismatch must not leave partial bytes");
    }

    // --- encode_data_row_into_with_formats の実バイナリ経路（WIRE-14）---

    #[test]
    fn data_row_binary_text_cell_uses_raw_utf8_bytes() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Text("lang".to_string())],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out).expect("encode");
        // 'D' + length(4) + field_count(2) = idx 7 から cell length(4)。
        let cell_len = i32_at(&out, 7) as usize;
        assert_eq!(cell_len, 4);
        assert_eq!(slice_at(&out, 11, cell_len), b"lang");
    }

    #[test]
    fn data_row_binary_null_cell_has_length_negative_one_regardless_of_format() {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out).expect("encode");
        assert_eq!(i32_at(&out, 7), -1);
    }

    #[test]
    fn data_row_binary_non_text_cell_is_rejected() {
        // `validate_binary_formats` を経由しない直接呼び出しでの契約違反
        // （バイナリ非対応列の実値セル）を fail-closed に拒否する。
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Integer(1)],
        };
        let mut out = b"KEEP".to_vec();
        let result = encode_data_row_into_with_formats(&row, &[FormatCode::Binary], &mut out);
        assert!(result.is_err());
        assert_eq!(out, b"KEEP", "failed encode must not leave partial bytes");
    }

    // --- column_binary_support（WIRE-14）---

    #[test]
    fn column_binary_support_matrix() {
        assert!(!column_binary_support(&ColumnMeta::Id));
        assert!(column_binary_support(&ColumnMeta::Scalar {
            name: "lang".to_string(),
            ty: engine::catalog::ColumnType::Text,
        }));
        assert!(!column_binary_support(&ColumnMeta::Scalar {
            name: "embedding".to_string(),
            ty: engine::catalog::ColumnType::Vector(3),
        }));
        assert!(!column_binary_support(&ColumnMeta::Computed {
            name: "expr".to_string(),
            ty: None,
        }));
        // `Computed{ty: Some(対応型)}` は対応、非対応型は非対応（Issue #1407）。
        for ty in [
            engine::catalog::ColumnType::BigInt,
            engine::catalog::ColumnType::Real,
            engine::catalog::ColumnType::Double,
            engine::catalog::ColumnType::Boolean,
            engine::catalog::ColumnType::Text,
        ] {
            assert!(column_binary_support(&ColumnMeta::Computed {
                name: "agg".to_string(),
                ty: Some(ty),
            }));
        }
        for ty in [
            engine::catalog::ColumnType::Numeric {
                precision: 20,
                scale: 0,
            },
            engine::catalog::ColumnType::Date,
            engine::catalog::ColumnType::Timestamp,
            engine::catalog::ColumnType::Vector(3),
        ] {
            assert!(!column_binary_support(&ColumnMeta::Computed {
                name: "agg".to_string(),
                ty: Some(ty),
            }));
        }
        // 数値・真偽値・bytea・uuid は対応（WIRE-14・Issue #1172）。
        for ty in [
            engine::catalog::ColumnType::Integer,
            engine::catalog::ColumnType::BigInt,
            engine::catalog::ColumnType::Boolean,
            engine::catalog::ColumnType::Real,
            engine::catalog::ColumnType::Double,
            engine::catalog::ColumnType::Bytea,
            engine::catalog::ColumnType::Uuid,
        ] {
            assert!(
                column_binary_support(&ColumnMeta::Scalar {
                    name: "col".to_string(),
                    ty: ty.clone(),
                }),
                "{ty:?} は binary 対応"
            );
        }
        // 非対応のまま（`0A000`）: 日時・JSON・NUMERIC（配列・ENUM は wire 結合テストで固定）。
        for ty in [
            engine::catalog::ColumnType::Date,
            engine::catalog::ColumnType::Timestamp,
            engine::catalog::ColumnType::Json,
            engine::catalog::ColumnType::Jsonb,
            engine::catalog::ColumnType::Numeric {
                precision: 5,
                scale: 2,
            },
        ] {
            assert!(
                !column_binary_support(&ColumnMeta::Scalar {
                    name: "col".to_string(),
                    ty: ty.clone(),
                }),
                "{ty:?} は binary 非対応のまま"
            );
        }
    }

    /// 新規型（Issue #895）のバイナリ指定が `validate_binary_formats` で
    /// `0A000`（`UnsupportedType`）に拒否されることを固定する（WIRE-14）。
    #[test]
    fn new_scalar_type_binary_request_is_rejected_as_feature_not_supported() {
        let columns = vec![ColumnMeta::Scalar {
            name: "d".to_string(),
            ty: engine::catalog::ColumnType::Date,
        }];
        let formats = ResultFormats::new(&[1])
            .resolve(columns.len())
            .expect("resolve");
        let err = validate_binary_formats(&columns, &formats).unwrap_err();
        assert_eq!(err, BinaryFormatError::UnsupportedType { column_index: 0 });
        assert_eq!(err.error_class().wire_code(), "0A000");
    }

    /// `NUMERIC` 列（TABLE-13〔検討中〕・TASK-197、Issue #885）は値の実体が
    /// `Cell::Numeric` であり、他の `Scalar` 列（`text` 固定）とは異なる
    /// `WireType::Numeric`（OID 1700・`"numeric"`）を公告しなければならない
    /// （PR #1020 codex-review 指摘。RowDescription の OID・
    /// `pg_type_name()` の双方を固定する。バイナリ非対応は
    /// `column_binary_support_matrix` 相当で別途保証されるためここでは
    /// 対象外）。
    #[test]
    fn numeric_scalar_column_wire_type_is_numeric_oid_1700_not_text() {
        let meta = ColumnMeta::Scalar {
            name: "price".to_string(),
            ty: engine::catalog::ColumnType::Numeric {
                precision: 5,
                scale: 2,
            },
        };
        let wire_type = column_wire_type(&meta);
        assert_eq!(wire_type.oid(), 1700);
        assert_eq!(wire_type.pg_type_name(), "numeric");
        assert_ne!(wire_type.oid(), WireType::Text.oid());
    }

    // --- 型 OID 写像の拡張（WIRE-13・TASK-200・Issue #895） ---

    /// [`WireType::ALL`] の全 variant が [`WireType::from_oid`] で往復する
    /// ことを固定する（単一情報源からの逆引きが正しいことの機械検証）。
    #[test]
    fn wire_type_all_round_trips_through_from_oid() {
        for wire_type in WireType::ALL {
            assert_eq!(
                WireType::from_oid(wire_type.oid()),
                Some(wire_type),
                "oid={}",
                wire_type.oid()
            );
        }
    }

    /// [`WireType::ALL`] 内で OID・型名がいずれも重複しないことを固定する
    /// （2 つの variant が同じ PostgreSQL 型を指してしまう設定ミスの検出）。
    #[test]
    fn wire_type_oids_and_names_are_unique() {
        let oids: Vec<i32> = WireType::ALL.iter().map(|t| t.oid()).collect();
        let mut sorted_oids = oids.clone();
        sorted_oids.sort_unstable();
        sorted_oids.dedup();
        assert_eq!(sorted_oids.len(), oids.len(), "OID が重複している");

        let names: Vec<&str> = WireType::ALL.iter().map(|t| t.pg_type_name()).collect();
        let mut sorted_names = names.clone();
        sorted_names.sort_unstable();
        sorted_names.dedup();
        assert_eq!(sorted_names.len(), names.len(), "型名が重複している");
    }

    /// `WireType::ALL` の要素数が [`WireType`] の variant 数（13）と一致する
    /// ことを固定する（実測値の回帰検知が目的。`ALL` 自体は
    /// [`wire_type_enum!`] マクロが enum 定義と同一の variant トークン列から
    /// 生成するため、`ALL` への追加漏れは PR #1037 codex-review 指摘の是正で
    /// 構造的に発生し得なくなった。このテストは「13 個という現在の想定数」
    /// が変わったことを検知する回帰テストであり、`ALL` の網羅性そのものの
    /// 保証はマクロが担う）。
    #[test]
    fn wire_type_all_is_exhaustive() {
        assert_eq!(WireType::ALL.len(), 13);
    }

    /// `PostgreSQL` 組み込みカタログの公開値（`pg_type.oid`／`typlen`／
    /// `typname`）との照合。本モジュールの `oid()`／`typlen()`／
    /// `pg_type_name()` とは独立にテスト側で固定表を持ち、正引きの値
    /// そのものを外部オラクルで検証する（WIRE-13・TASK-200・Issue #895）。
    #[test]
    fn wire_type_table_matches_postgres_builtin_catalog() {
        let expected: [(WireType, i32, i16, &str); 13] = [
            (WireType::Numeric, 1700, -1, "numeric"),
            (WireType::Text, 25, -1, "text"),
            (WireType::Int4, 23, 4, "int4"),
            (WireType::Int8, 20, 8, "int8"),
            (WireType::Bool, 16, 1, "bool"),
            (WireType::Float4, 700, 4, "float4"),
            (WireType::Float8, 701, 8, "float8"),
            (WireType::Date, 1082, 4, "date"),
            (WireType::Timestamp, 1114, 8, "timestamp"),
            (WireType::Bytea, 17, -1, "bytea"),
            (WireType::Uuid, 2950, 16, "uuid"),
            (WireType::Json, 114, -1, "json"),
            (WireType::Jsonb, 3802, -1, "jsonb"),
        ];
        for (wire_type, oid, typlen, name) in expected {
            assert_eq!(wire_type.oid(), oid, "oid mismatch for {name}");
            assert_eq!(wire_type.typlen(), typlen, "typlen mismatch for {name}");
            assert_eq!(wire_type.pg_type_name(), name, "name mismatch for {oid}");
        }
    }

    /// 未知の OID は fail-closed に `None` を返す（`0`・負値・PostgreSQL の
    /// 他の組み込み型〔`varchar` = 1043〕・`i32::MAX` を確認）。
    #[test]
    fn from_oid_unknown_is_none() {
        for oid in [0, -1, 1043, i32::MAX] {
            assert_eq!(WireType::from_oid(oid), None, "oid={oid}");
        }
    }

    /// `column_wire_type` が全 `ColumnType`（`VECTOR`／`ARRAY`／`ENUM`／
    /// `NUMERIC` を含む）＋`Id`＋`Computed` について期待どおりの `WireType`
    /// を返すことを固定する。`Id`→`Numeric`・`Text`→`Text`・`Vector`→`Text`
    /// が不変であること（受入基準 3）を明示アサートする（WIRE-13・
    /// TASK-200・Issue #895）。
    #[test]
    fn column_wire_type_matrix() {
        use engine::catalog::ColumnType;

        let scalar = |name: &str, ty: ColumnType| ColumnMeta::Scalar {
            name: name.to_string(),
            ty,
        };

        // 受入基準 3: 既存の `id`／`TEXT`／`VECTOR` の公告は不変。
        assert_eq!(column_wire_type(&ColumnMeta::Id), WireType::Numeric);
        assert_eq!(
            column_wire_type(&scalar("lang", ColumnType::Text)),
            WireType::Text
        );
        assert_eq!(
            column_wire_type(&scalar("embedding", ColumnType::Vector(3))),
            WireType::Text
        );

        // 新規 OID 公告（本 Issue の対象）。
        assert_eq!(
            column_wire_type(&scalar("flag", ColumnType::Boolean)),
            WireType::Bool
        );
        assert_eq!(
            column_wire_type(&scalar("n", ColumnType::Integer)),
            WireType::Int4
        );
        assert_eq!(
            column_wire_type(&scalar("b", ColumnType::BigInt)),
            WireType::Int8
        );
        assert_eq!(
            column_wire_type(&scalar("r", ColumnType::Real)),
            WireType::Float4
        );
        assert_eq!(
            column_wire_type(&scalar("d", ColumnType::Double)),
            WireType::Float8
        );
        assert_eq!(
            column_wire_type(&scalar("dt", ColumnType::Date)),
            WireType::Date
        );
        assert_eq!(
            column_wire_type(&scalar("ts", ColumnType::Timestamp)),
            WireType::Timestamp
        );
        assert_eq!(
            column_wire_type(&scalar("blob", ColumnType::Bytea)),
            WireType::Bytea
        );
        assert_eq!(
            column_wire_type(&scalar("uid", ColumnType::Uuid)),
            WireType::Uuid
        );
        assert_eq!(
            column_wire_type(&scalar("doc", ColumnType::Json)),
            WireType::Json
        );
        assert_eq!(
            column_wire_type(&scalar("docb", ColumnType::Jsonb)),
            WireType::Jsonb
        );
        assert_eq!(
            column_wire_type(&scalar(
                "price",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2
                }
            )),
            WireType::Numeric
        );

        // 据え置き判断（受入基準外）: `ARRAY`／`ENUM` は専用 OID を持たず
        // `text` のまま。
        let array_ty = engine::catalog::ArrayType::new(engine::catalog::ArrayElemType::Text, 4)
            .expect("valid array type");
        assert_eq!(
            column_wire_type(&scalar("tags", ColumnType::Array(array_ty))),
            WireType::Text
        );

        let db_path = std::env::temp_dir().join(format!(
            "result-encoder-column-wire-type-matrix-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        let storage = engine::storage::Storage::open(&db_path).expect("open throwaway storage");
        let enum_def = storage
            .create_enum_type("mood", vec!["happy".to_string()])
            .expect("create enum type");
        assert_eq!(
            column_wire_type(&scalar("mood", ColumnType::Enum(enum_def))),
            WireType::Text
        );
        drop(storage);
        let _ = std::fs::remove_file(&db_path);

        assert_eq!(
            column_wire_type(&ColumnMeta::Computed {
                name: "expr".to_string(),
                ty: None,
            }),
            WireType::Text
        );
    }

    /// `RowDescription` が新規スカラー型（BOOLEAN／REAL／DOUBLE
    /// PRECISION／DATE／TIMESTAMP／BYTEA／UUID／JSON／JSONB）を PostgreSQL
    /// 組み込み OID・typlen で正しく公告することを、生成バイト列から直接
    /// 読み取って固定する（WIRE-13・TASK-200・Issue #895）。
    #[test]
    fn row_description_encodes_new_scalar_types_with_builtin_oids() {
        use engine::catalog::ColumnType;

        let columns = vec![
            ColumnMeta::Scalar {
                name: "flag".to_string(),
                ty: ColumnType::Boolean,
            },
            ColumnMeta::Scalar {
                name: "r".to_string(),
                ty: ColumnType::Real,
            },
            ColumnMeta::Scalar {
                name: "d".to_string(),
                ty: ColumnType::Double,
            },
            ColumnMeta::Scalar {
                name: "dt".to_string(),
                ty: ColumnType::Date,
            },
            ColumnMeta::Scalar {
                name: "ts".to_string(),
                ty: ColumnType::Timestamp,
            },
            ColumnMeta::Scalar {
                name: "blob".to_string(),
                ty: ColumnType::Bytea,
            },
            ColumnMeta::Scalar {
                name: "uid".to_string(),
                ty: ColumnType::Uuid,
            },
            ColumnMeta::Scalar {
                name: "doc".to_string(),
                ty: ColumnType::Json,
            },
            ColumnMeta::Scalar {
                name: "docb".to_string(),
                ty: ColumnType::Jsonb,
            },
        ];
        let expected: [(i32, i16); 9] = [
            (16, 1),    // bool
            (700, 4),   // float4
            (701, 8),   // float8
            (1082, 4),  // date
            (1114, 8),  // timestamp
            (17, -1),   // bytea
            (2950, 16), // uuid
            (114, -1),  // json
            (3802, -1), // jsonb
        ];

        let msg = encode_row_description(&columns).expect("encode");
        assert_eq!(i16_at(&msg, 5), 9);

        // body: 'T' + length(4) + field_count(2) から各フィールドが続く。
        let mut cursor = 1 + 4 + 2;
        for (name, (expected_oid, expected_typlen)) in
            columns.iter().map(column_name).zip(expected.iter())
        {
            let name_end = msg[cursor..]
                .iter()
                .position(|&b| b == 0)
                .expect("NUL terminator");
            let actual_name =
                std::str::from_utf8(slice_at(&msg, cursor, name_end)).expect("utf8 name");
            assert_eq!(actual_name, name);
            cursor += name_end + 1;
            cursor += 4 + 2; // table_oid + attnum
            let oid = i32_at(&msg, cursor);
            assert_eq!(oid, *expected_oid, "column={name}");
            cursor += 4;
            let typlen = i16_at(&msg, cursor);
            assert_eq!(typlen, *expected_typlen, "column={name}");
            cursor += 2 + 4 + 2; // typlen + typmod + format
        }
    }

    // --- 列メタ付き DataRow エンコード（WIRE-14・Issue #1172）---

    fn scalar(ty: engine::catalog::ColumnType) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: "c".to_string(),
            ty,
        }
    }

    fn one_cell_binary(
        ty: engine::catalog::ColumnType,
        cell: Cell,
    ) -> Result<Vec<u8>, EncodeError> {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![cell],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_columns(&row, &[scalar(ty)], &[FormatCode::Binary], &mut out)?;
        // 'D' + len(4) + field_count(2) の後ろ（長さ付きセル）を返す。
        Ok(out.get(7..).unwrap_or_default().to_vec())
    }

    fn with_len(payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as i32).to_be_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn binary_typed_cells_golden_bytes() {
        use engine::catalog::ColumnType as T;
        assert_eq!(
            one_cell_binary(T::Integer, Cell::SignedInteger(-1)).unwrap(),
            with_len(&[0xff; 4])
        );
        assert_eq!(
            one_cell_binary(T::BigInt, Cell::SignedInteger(i64::MIN)).unwrap(),
            with_len(&i64::MIN.to_be_bytes())
        );
        assert_eq!(
            one_cell_binary(T::Real, Cell::Float(f64::from(0.1f32))).unwrap(),
            with_len(&0.1f32.to_bits().to_be_bytes())
        );
        assert_eq!(
            one_cell_binary(T::Double, Cell::Float(1.0)).unwrap(),
            with_len(&1.0f64.to_bits().to_be_bytes())
        );
        assert_eq!(
            one_cell_binary(T::Boolean, Cell::Bool(true)).unwrap(),
            with_len(&[1])
        );
        assert_eq!(
            one_cell_binary(T::Boolean, Cell::Bool(false)).unwrap(),
            with_len(&[0])
        );
        assert_eq!(
            one_cell_binary(T::Bytea, Cell::Bytes(Vec::new())).unwrap(),
            with_len(&[])
        );
        assert_eq!(
            one_cell_binary(T::Bytea, Cell::Bytes(vec![0, 1, 0xff])).unwrap(),
            with_len(&[0, 1, 0xff])
        );
        let u = engine::uuid::Uuid::from_bytes([7u8; 16]);
        assert_eq!(
            one_cell_binary(T::Uuid, Cell::Uuid(u)).unwrap(),
            with_len(&[7u8; 16])
        );
    }

    #[test]
    fn binary_typed_null_is_minus_one_for_every_type() {
        use engine::catalog::ColumnType as T;
        for ty in [
            T::Integer,
            T::BigInt,
            T::Real,
            T::Double,
            T::Boolean,
            T::Bytea,
            T::Uuid,
        ] {
            assert_eq!(
                one_cell_binary(ty, Cell::Null).unwrap(),
                (-1i32).to_be_bytes().to_vec()
            );
        }
    }

    #[test]
    fn binary_typed_failures_are_fail_closed_and_leave_out_untouched() {
        use engine::catalog::ColumnType as T;
        let cases = [
            (T::Integer, Cell::SignedInteger(i64::from(i32::MAX) + 1)),
            (T::Real, Cell::Float(0.1f64)),
            (T::Integer, Cell::Text("1".to_string())),
            (T::Bytea, Cell::Text("x".to_string())),
            (T::Uuid, Cell::Bool(true)),
            (T::Date, Cell::Integer(1)),
        ];
        for (ty, cell) in cases {
            let row = ResultRow {
                id: 1,
                score: 0.0,
                cells: vec![cell],
            };
            let mut out = b"KEEP".to_vec();
            let r = encode_data_row_into_with_columns(
                &row,
                &[scalar(ty.clone())],
                &[FormatCode::Binary],
                &mut out,
            );
            assert!(r.is_err(), "{ty:?}");
            assert_eq!(out, b"KEEP");
        }
    }

    fn computed(ty: engine::catalog::ColumnType) -> ColumnMeta {
        ColumnMeta::Computed {
            name: "c".to_string(),
            ty: Some(ty),
        }
    }

    fn one_computed_binary(
        ty: engine::catalog::ColumnType,
        cell: Cell,
        out: &mut Vec<u8>,
    ) -> Result<(), EncodeError> {
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![cell],
        };
        encode_data_row_into_with_columns(&row, &[computed(ty)], &[FormatCode::Binary], out)
    }

    /// Issue #1407: `COUNT`・`ROW_NUMBER` 系の `Computed{BigInt}`＋`Cell::Integer(u64)` は
    /// int8 で出る。`i64` 範囲外は fail-closed で部分フレームを残さない。
    #[test]
    fn binary_computed_bigint_accepts_unsigned_integer_cell() {
        use engine::catalog::ColumnType as T;
        let mut out = Vec::new();
        one_computed_binary(T::BigInt, Cell::Integer(5), &mut out).unwrap();
        let mut expected = vec![b'D', 0, 0, 0, 18, 0, 1];
        expected.extend_from_slice(&with_len(&5i64.to_be_bytes()));
        assert_eq!(out, expected);

        let mut out = b"KEEP".to_vec();
        assert!(one_computed_binary(T::BigInt, Cell::Integer(u64::MAX), &mut out).is_err());
        assert_eq!(out, b"KEEP");
    }

    /// Issue #1407: `Computed{Real}` は f32 へ無損失な `Cell::Float` を float4 で出し、
    /// 型と Cell の不一致（`Text`＋`SignedInteger`）は fail-closed のまま。
    #[test]
    fn binary_computed_real_and_mismatch() {
        use engine::catalog::ColumnType as T;
        let mut out = Vec::new();
        one_computed_binary(T::Real, Cell::Float(f64::from(0.5f32)), &mut out).unwrap();
        assert_eq!(out.get(7..).unwrap(), with_len(&0.5f32.to_be_bytes()));

        let mut out = b"KEEP".to_vec();
        assert!(one_computed_binary(T::Text, Cell::SignedInteger(1), &mut out).is_err());
        assert_eq!(out, b"KEEP");
    }

    #[test]
    fn binary_typed_mixed_formats_apply_per_column() {
        use engine::catalog::ColumnType as T;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::SignedInteger(5), Cell::SignedInteger(5)],
        };
        let mut out = Vec::new();
        encode_data_row_into_with_columns(
            &row,
            &[scalar(T::Integer), scalar(T::Integer)],
            &[FormatCode::Text, FormatCode::Binary],
            &mut out,
        )
        .unwrap();
        let mut expected = with_len(b"5");
        expected.extend(with_len(&5i32.to_be_bytes()));
        assert_eq!(out.get(7..).unwrap(), expected.as_slice());
    }

    #[test]
    fn with_columns_all_text_matches_legacy_encoder() {
        use engine::catalog::ColumnType as T;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::SignedInteger(-3), Cell::Bool(true), Cell::Null],
        };
        let cols = [scalar(T::Integer), scalar(T::Boolean), scalar(T::Bytea)];
        let mut a = Vec::new();
        encode_data_row_into_with_columns(&row, &cols, &[FormatCode::Text; 3], &mut a).unwrap();
        assert_eq!(a, encode_data_row(&row).unwrap());
    }

    #[test]
    fn with_columns_length_mismatch_is_error() {
        use engine::catalog::ColumnType as T;
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Bool(true)],
        };
        let mut out = Vec::new();
        assert!(
            encode_data_row_into_with_columns(&row, &[], &[FormatCode::Binary], &mut out).is_err()
        );
        assert!(
            encode_data_row_into_with_columns(&row, &[scalar(T::Boolean)], &[], &mut out).is_err()
        );
    }

    #[test]
    fn wire_type_supports_binary_matrix() {
        for t in WireType::ALL {
            let expected = matches!(
                t,
                WireType::Text
                    | WireType::Int4
                    | WireType::Int8
                    | WireType::Bool
                    | WireType::Float4
                    | WireType::Float8
                    | WireType::Bytea
                    | WireType::Uuid
            );
            assert_eq!(t.supports_binary(), expected, "{t:?}");
        }
    }
}
