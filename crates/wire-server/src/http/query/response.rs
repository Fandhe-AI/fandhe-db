//! `QueryResult`（`search`／`scan`／`aggregate` 成功時。engine 側 SQL 表層の
//! 結果セット形状。ポインタ: SQL-1〜4・SQL-13〜15）→ NoSQL 表層の JSON 応答本文
//! `{"columns":[{"name":...,"type":...}],"rows":[[...],...],"row_count":n}`
//! への写像（Issue #762。対象ビヘイビア TASK-181・NOSQL-11。ポインタ:
//! `docs/spec/05-tasks.md` TASK-181・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-11）。
//!
//! 責務境界: [`crate::http::error_body`] の成功系対応物であり、同じく
//! 「本文 `String` を返すまで」に閉じる純関数を提供する。ステータス行・
//! `Connection: close`・`Content-Type`／`Content-Length`・CRLF の組み立てや
//! ソケット I/O は含まない（応答エンベロープは #746 の責務）。
//!
//! `explain: true` 時の `{"explain":["<行>", ...]}` 応答は [`encode_explain`]
//! が担う（TASK-186・NOSQL-10。Issue #765）。`encode`（`columns`／`rows`／
//! `row_count`）とは独立の関数として分離し、`search`／`scan`／`aggregate` の
//! 通常応答へ影響を与えない。
//!
//! スコープ外（隣接 Issue と二重実装しない）:
//! - `score` の送出有無・`columns` 指定に応じた投影の絞り込み（#763・#766。
//!   本モジュールは `QueryResult.columns`／`rows[].cells` をそのまま写像し、
//!   `ResultRow.score`／`id` を独自に付加しない）
//! - 接続ハンドラからの呼び出し結線（#747／#758 以降）
//!
//! `columns[].type` の型名は SQL 表層の `RowDescription`
//! （[`crate::result_encoder::encode_row_description`]）が公告する PostgreSQL
//! 型名と**同一の対応表**（[`crate::result_encoder::WireType`]）を共有する
//! （`Id`・`Scalar`）。`VECTOR` 列は wire 側と同じく `"text"` として公告される
//! 一方、値そのものは wire の text フォーマットではなく native JSON（配列・数値・
//! 真偽値）で返す——**型名は wire 側との同一性を、値表現は JSON としての
//! ロスレス性をそれぞれ優先する意図的な非対称**であり、値表現を型名に合わせて
//! 文字列化する「修正」はしないこと。
//!
//! `Computed`（集計・式項目）列は、wire 側 `RowDescription` が Issue #1173 で
//! 静的な結果型の OID（`COUNT` → `int8` など）を公告するのに対し、本表層は
//! NOSQL-11 の契約に従い `"text"` 固定のまま据え置く（wire 側との**意図的な非対称**。
//! 計算列の型名化は NOSQL-11／NOSQL-17 の spec 判断が必要なため別途扱う）。
//!
//! 値表現（`Cell` → JSON）:
//! - `Cell::Null` → `null`
//! - `Cell::Integer(u64)` → JSON number（10進テキスト。`u64::MAX` まで桁落ち
//!   なく表現できるが、2^53 を超える値を JS 系クライアントが `JSON.parse`
//!   すると仮数部の精度により丸められうる。文字列化への変更は spec 側判断
//!   に委ねる〔TASK-185〕。表現自体は RFC 8259 上有効な JSON number）
//! - `Cell::Float(f64)` → JSON number。**非有限（NaN／±∞）は `null` へ丸め
//!   ず `Err` で fail-closed**（`"NaN"`／`"Infinity"` は妥当な JSON number
//!   ではないため）。engine は評価時に非有限を `22000` で拒否する契約だが、
//!   本モジュールは多層防御として同じ制約を持つ
//! - `Cell::Bool(bool)` → `true`／`false`（native JSON 真偽値）
//! - `Cell::Date(i32)`／`Cell::Timestamp(i64)` → JSON string（ISO テキスト。
//!   `engine::datetime::format_date`／`format_timestamp` へ委譲。TABLE-13・
//!   TASK-197、Issue #884）
//! - `Cell::Text(String)` → JSON string。[`crate::http::error_body::
//!   escape_json_string_into`] を列名と共通で再利用する
//! - `Cell::Vector(Vec<f32>)` → JSON array of number（要素は `f32::to_string()`。
//!   `crate::result_encoder::cell_to_text` の `[1,2.5]` 表現と要素の数値表記が
//!   一致する）。要素の非有限も同様に `Err`
//! - `Cell::Bytes(Vec<u8>)` → JSON string（標準 base64・パディングあり。
//!   `crate::http::query::base64_std::encode_base64_std`。wire 側の `\x` 16 進
//!   テキスト表現とは異なる——値表現は表層ごとの規約〔B7〕であり、`BYTEA` に
//!   限り型名の wire 側同一性より JSON との親和性を優先する。Issue #886）
//!
//! 出力不変条件: [`crate::http::error_body::encode`] と同じくキー順固定
//! （`columns` → `rows` → `row_count`）・空白なし・0x20 未満のバイトを含まない
//! （`Content-Length`〔#746〕算出対象を安定させる）。

use std::fmt::Write as _;

use engine::catalog::ColumnType;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};

use crate::http::error_body::escape_json_string_into;

/// `2^53 - 1`（IEEE 754 倍精度浮動小数点数が整数として無損失に表現できる
/// 上限。JS 系 JSON クライアントの `Number.MAX_SAFE_INTEGER` と同じ値）。
/// これを超える `Cell::SignedInteger`（`BIGINT` 列。Issue #896）は精度の
/// 誤解を避けるため JSON 文字列で送出する（`Cell::Integer(u64)`〔`id`／
/// `COUNT`〕は TASK-185 の担当のまま本 Issue では変更しない）。
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// [`encode`] の失敗を表す（`Cell::Float`／`Cell::Vector` の非有限要素が
/// JSON で表現できない場合のみ発生する）。他テナント情報・内部詳細を運ばない
/// ため `Debug` のみ持つ（`crate::result_encoder::EncodeError` と同じ方針）。
#[derive(Debug)]
pub struct ResponseEncodeError;

impl ClassifiedError for ResponseEncodeError {
    fn error_class(&self) -> ErrorClass {
        ErrorClass::InternalError
    }

    fn client_message(&self) -> String {
        // `ErrorClass::InternalError` は `WireError::new` 側で固定文言へ
        // 差し替えられる契約（`engine::error_format` 参照）のため、ここでの
        // 文言自体は到達しても情報漏えいにならないが、内部詳細（どの列・
        // どの値が非有限だったか）は含めない。
        "internal error".to_string()
    }
}

/// 1 セルぶんの JSON 値表現を `out` へ追記する。
fn write_cell(out: &mut String, cell: &Cell) -> Result<(), ResponseEncodeError> {
    match cell {
        Cell::Null => {
            out.push_str("null");
            Ok(())
        }
        Cell::Integer(v) => {
            // `write!` への `String` 追記は infallible（アロケーション失敗
            // 以外で失敗しない）ため戻り値は捨ててよい（`error_body.rs` と
            // 同じ方針）。
            let _ = write!(out, "{v}");
            Ok(())
        }
        // `INTEGER`／`BIGINT` 列の投影結果（Issue #881・TABLE-13・TASK-196）。
        // `INTEGER`（`i32` を無損失に格上げ）は常に安全範囲に収まるが、
        // `BIGINT`（`i64` 全域）は超えうるため、`|v| > MAX_SAFE_INTEGER` は
        // JSON 文字列で送出する（Issue #896。JS 系クライアントが
        // `JSON.parse` した際の精度誤解を防ぐ。`Cell::Integer(u64)`〔`id`／
        // `COUNT`〕は TASK-185 の担当のまま変更しない）。
        Cell::SignedInteger(v) => {
            if v.unsigned_abs() > MAX_SAFE_INTEGER.unsigned_abs() {
                out.push('"');
                let _ = write!(out, "{v}");
                out.push('"');
            } else {
                let _ = write!(out, "{v}");
            }
            Ok(())
        }
        Cell::Float(f) => write_finite_f64(out, *f),
        Cell::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            Ok(())
        }
        Cell::Date(days) => {
            out.push('"');
            escape_json_string_into(out, &engine::datetime::format_date(*days));
            out.push('"');
            Ok(())
        }
        Cell::Timestamp(micros) => {
            out.push('"');
            escape_json_string_into(out, &engine::datetime::format_timestamp(*micros));
            out.push('"');
            Ok(())
        }
        Cell::Text(s) => {
            out.push('"');
            escape_json_string_into(out, s);
            out.push('"');
            Ok(())
        }
        Cell::Vector(values) => {
            out.push('[');
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_finite_f32(out, *v)?;
            }
            out.push(']');
            Ok(())
        }
        Cell::Array(array_value) => write_array(out, array_value),
        Cell::Bytes(bytes) => {
            // `BYTEA` の JSON 表現は標準 base64（RFC 4648 §4・パディングあり。
            // B7・Issue #886）。base64 のアルファベットは JSON エスケープ不要。
            out.push('"');
            out.push_str(&crate::http::query::base64_std::encode_base64_std(bytes));
            out.push('"');
            Ok(())
        }
        Cell::Json(text) => {
            // `JSON`／`JSONB` は native JSON 値として出力する（Issue #889 D6）。
            // 格納テキストを共有パーサー（`engine::json::parse_json`）で再パース
            // し `write_canonical` で再シリアライズしてから埋め込む——格納バイト
            // 列を生のまま応答本文へ連結しない安全側の設計（security.md
            // 「不安全な設計」対応。連結は非構造化テキストの injection 相当に
            // なりうる）。再パース失敗は格納契約違反（`row_codec` の encode
            // チョークポイントが常に検証済みテキストのみを格納する契約に反する）
            // であり内部エラー（`XX000`）として fail-closed に扱う。
            let value = engine::json::parse_json(text).map_err(|_| ResponseEncodeError)?;
            engine::json::write_canonical(&value, out);
            Ok(())
        }
        Cell::Numeric(d) => {
            // 正規テキスト表現（`Decimal::Display`。先頭ゼロ・指数表記・
            // `-0` を持たない）をそのまま JSON number として書く（D8。
            // TABLE-13〔検討中〕・TASK-197、Issue #885）。精度は失わない。
            let _ = write!(out, "{d}");
            Ok(())
        }
        Cell::Uuid(u) => {
            // 正規テキスト表現（小文字 `8-4-4-4-12`）を JSON string として書く
            // （U4。TABLE-13〔検討中〕・TASK-197、Issue #887）。
            out.push('"');
            escape_json_string_into(out, &u.to_string());
            out.push('"');
            Ok(())
        }
    }
}

/// 配列セルの JSON 表現（Issue #888・#1193）。NULL 要素は `null`。整数は JSON number
/// （`BIGINT` は安全整数を超えたら文字列。`Cell::SignedInteger` と同じ規則）、
/// `REAL`／`DOUBLE PRECISION` は有限値のみ JSON number（非有限は fail-closed に `Err`）、
/// `DATE`／`TIMESTAMP`／`UUID` は文字列、`BOOLEAN` は真偽値。
fn write_array(
    out: &mut String,
    array_value: &engine::row_codec::ArrayValue,
) -> Result<(), ResponseEncodeError> {
    use engine::row_codec::ArrayValue;

    fn each<T>(
        out: &mut String,
        items: &[Option<T>],
        mut f: impl FnMut(&mut String, &T) -> Result<(), ResponseEncodeError>,
    ) -> Result<(), ResponseEncodeError> {
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match item {
                None => out.push_str("null"),
                Some(v) => f(out, v)?,
            }
        }
        Ok(())
    }

    fn quoted(out: &mut String, text: &str) {
        out.push('"');
        escape_json_string_into(out, text);
        out.push('"');
    }

    out.push('[');
    match array_value {
        ArrayValue::Text(items) => each(out, items, |out, s| {
            quoted(out, s);
            Ok(())
        })?,
        ArrayValue::Bool(items) => each(out, items, |out, b| {
            out.push_str(if *b { "true" } else { "false" });
            Ok(())
        })?,
        ArrayValue::Integer(items) => each(out, items, |out, v| {
            let _ = write!(out, "{v}");
            Ok(())
        })?,
        ArrayValue::BigInt(items) => each(out, items, |out, v| {
            if v.unsigned_abs() > MAX_SAFE_INTEGER.unsigned_abs() {
                out.push('"');
                let _ = write!(out, "{v}");
                out.push('"');
            } else {
                let _ = write!(out, "{v}");
            }
            Ok(())
        })?,
        ArrayValue::Real(items) => each(out, items, |out, v| write_finite_f32(out, *v))?,
        ArrayValue::Double(items) => each(out, items, |out, v| write_finite_f64(out, *v))?,
        ArrayValue::Date(items) => each(out, items, |out, v| {
            quoted(out, &engine::datetime::format_date(*v));
            Ok(())
        })?,
        ArrayValue::Timestamp(items) => each(out, items, |out, v| {
            quoted(out, &engine::datetime::format_timestamp(*v));
            Ok(())
        })?,
        ArrayValue::Uuid(items) => each(out, items, |out, v| {
            quoted(out, &v.to_string());
            Ok(())
        })?,
        // Issue #1357: スカラー列の `Cell::Numeric`／`Cell::Bytes`／`Cell::Json`／
        // `Cell::Text`（ENUM）と同じ JSON 表現を要素ごとに使う。
        ArrayValue::Numeric(items) => each(out, items, |out, d| {
            let _ = write!(out, "{d}");
            Ok(())
        })?,
        ArrayValue::Bytea(items) => each(out, items, |out, b| {
            out.push('"');
            out.push_str(&crate::http::query::base64_std::encode_base64_std(b));
            out.push('"');
            Ok(())
        })?,
        // JSON／JSONB 要素は再パース＋`write_canonical` で埋め込み、格納テキストを
        // 生連結しない（`Cell::Json` と同じ安全側の設計）。再パース失敗は fail-closed。
        ArrayValue::Json(items) | ArrayValue::Jsonb(items) => each(out, items, |out, text| {
            let value = engine::json::parse_json(text).map_err(|_| ResponseEncodeError)?;
            engine::json::write_canonical(&value, out);
            Ok(())
        })?,
        ArrayValue::Enum(items) => each(out, items, |out, label| {
            quoted(out, label);
            Ok(())
        })?,
    }
    out.push(']');
    Ok(())
}

/// 有限 `f64` を JSON number として書く。非有限（NaN／±∞）は fail-closed に
/// `Err` とする（JSON はこれらを表現できないため。モジュールドキュメント参照）。
fn write_finite_f64(out: &mut String, f: f64) -> Result<(), ResponseEncodeError> {
    if !f.is_finite() {
        return Err(ResponseEncodeError);
    }
    let _ = write!(out, "{f}");
    Ok(())
}

/// [`write_finite_f64`] の `f32`（`Cell::Vector` の要素）版。
fn write_finite_f32(out: &mut String, f: f32) -> Result<(), ResponseEncodeError> {
    if !f.is_finite() {
        return Err(ResponseEncodeError);
    }
    let _ = write!(out, "{f}");
    Ok(())
}

/// NoSQL 表層 `columns[].type` の型名（Issue #896・NOSQL-17。
/// `crate::result_encoder::WireType`／`column_wire_type`（SQL wire
/// `RowDescription` の OID 写像。Issue #895 の担当）とは**独立**の対応表——
/// SQL wire 側は多くの新型を後方互換のため `text`（OID 25）へ丸めるが、
/// NoSQL の JSON API は型情報をクライアントへそのまま伝える方が有用なため、
/// 列型ごとに個別の名前を返す（BREAKING CHANGE。既存列の出力値が変わる）。
/// 網羅 `match`（ワイルドカードなし）にすることで将来の列型追加を
/// コンパイルエラーで検出する。
fn nosql_type_name(meta: &ColumnMeta) -> &'static str {
    match meta {
        // `id` 疑似列は SQL wire 側（`column_wire_type`。Issue #895）と型名が
        // 一致する意図的な設計（他の集計等の `numeric` 系 API との一貫性）であり、
        // ここでのみ SQL wire 側の対応表へ委譲することで二重管理を避ける
        // （`Scalar` 腕は列型ごとに独立に決める。モジュール doc「独立の対応表」参照）。
        ColumnMeta::Id => crate::result_encoder::column_wire_type(meta).pg_type_name(),
        // 式項目・集計結果（`Computed`）は wire 側が Issue #1173 で型付き OID を
        // 公告するようになっても、NOSQL-11 の契約に従い `"text"` 固定を維持する
        // （wire 側との意図的な非対称。モジュール doc 参照）。
        ColumnMeta::Computed { .. } => "text",
        ColumnMeta::Scalar { ty, .. } => match ty {
            ColumnType::Text => "text",
            ColumnType::Vector(_) => "vector",
            ColumnType::Integer => "integer",
            ColumnType::BigInt => "bigint",
            ColumnType::Real => "real",
            ColumnType::Double => "double precision",
            ColumnType::Boolean => "boolean",
            ColumnType::Date => "date",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Numeric { .. } => "numeric",
            ColumnType::Uuid => "uuid",
            ColumnType::Bytea => "bytea",
            ColumnType::Json => "json",
            ColumnType::Jsonb => "jsonb",
            // 固定語彙の ENUM 型は語彙一覧ではなく種別のみを返す（個々の
            // ENUM 型名・語彙は型名の安定性のため含めない設計判断）。
            ColumnType::Enum(_) => "enum",
            ColumnType::Array(array_ty) => match array_ty.elem() {
                engine::catalog::ArrayElemType::Text => "text[]",
                engine::catalog::ArrayElemType::Bool => "boolean[]",
                engine::catalog::ArrayElemType::Integer => "integer[]",
                engine::catalog::ArrayElemType::BigInt => "bigint[]",
                engine::catalog::ArrayElemType::Real => "real[]",
                engine::catalog::ArrayElemType::Double => "double precision[]",
                engine::catalog::ArrayElemType::Date => "date[]",
                engine::catalog::ArrayElemType::Timestamp => "timestamp[]",
                engine::catalog::ArrayElemType::Uuid => "uuid[]",
                engine::catalog::ArrayElemType::Numeric { .. } => "numeric[]",
                engine::catalog::ArrayElemType::Bytea => "bytea[]",
                engine::catalog::ArrayElemType::Json => "json[]",
                engine::catalog::ArrayElemType::Jsonb => "jsonb[]",
                engine::catalog::ArrayElemType::Enum => "enum[]",
            },
        },
    }
}

/// `columns[]` の 1 要素 `{"name":"...","type":"..."}` を `out` へ追記する。
/// `name` はカタログ由来の untrusted 文字列（DDL でユーザーが指定した列名）
/// のため [`Cell::Text`] と同じエスケーパを通す。
fn write_column(out: &mut String, meta: &ColumnMeta) {
    out.push_str("{\"name\":\"");
    escape_json_string_into(out, crate::result_encoder::column_name(meta));
    out.push_str("\",\"type\":\"");
    // `nosql_type_name` はいずれの列型でも固定 ASCII
    // （`"numeric"`／`"text"`／`"bool"`／`"jsonb"` 等。WIRE-13・TASK-200・
    // Issue #895・本 Issue #896 で追加した新型も同様）のためエスケープ不要
    // だが、型写像表が将来拡張されても安全側に倒れるよう一律エスケーパを通す
    // （`error_body.rs::wire_code_and_label_never_require_escaping` と
    // 同じ考え方）。
    escape_json_string_into(out, nosql_type_name(meta));
    out.push_str("\"}");
}

/// `QueryResult` → `{"columns":[...],"rows":[...],"row_count":n}`
/// （キー順固定・空白なしのコンパクト形）。
///
/// `row_count` は常に `result.rows.len()` と一致する（別経路で算出しない）。
/// 事前確保は行数 × 定数の概算ヒントに留め、無制限確保はしない
/// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub fn encode(result: &QueryResult) -> Result<String, ResponseEncodeError> {
    // 概算ヒント: 列 1 個あたり約 24 バイト（name+type の固定オーバーヘッド）、
    // 行 1 個あたり列数 × 8 バイト（数値・区切り文字の目安）。実データが
    // これを超えても `String` は再確保して継続するだけであり、上限として
    // 機能するものではない（無制限確保の防止は呼び出し元・engine 側の
    // `LIMIT` 契約が担う）。
    let capacity_hint = 32
        + result.columns.len().saturating_mul(24)
        + result
            .rows
            .len()
            .saturating_mul(result.columns.len().saturating_mul(8).max(8));
    let mut out = String::with_capacity(capacity_hint);

    out.push_str("{\"columns\":[");
    for (i, meta) in result.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_column(&mut out, meta);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in result.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, cell) in row.cells.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_cell(&mut out, cell)?;
        }
        out.push(']');
    }
    out.push_str("],\"row_count\":");
    let _ = write!(out, "{}", result.rows.len());
    out.push('}');

    Ok(out)
}

/// `explain: true` の `search` 要求が返す `EXPLAIN` 応答本文
/// `{"explain":["<行>", ...]}`（TASK-186・NOSQL-10。Issue #765）への写像。
///
/// `result` は [`crate::core::EngineCore::explain_bound_plan_in_session`]（SQL
/// `EXPLAIN SELECT ... USING PLAN(...)` の `Statement::Explain` アームと同一の
/// 私的ヘルパーを共有する。`crate::core::EngineCore` モジュールドキュメント参照）
/// の戻り値をそのまま渡す想定で、列が `Computed { name: "QUERY PLAN" }` 1 本・
/// 各行が [`Cell::Text`] 1 個であることを検証してから写像する。この形状
/// （`sql::explain::build_explain_result` の固定契約）から逸脱する場合は
/// best-effort に描画せず `Err`（`ErrorClass::InternalError`）で fail-closed に
/// 拒否する。
pub fn encode_explain(result: &QueryResult) -> Result<String, ResponseEncodeError> {
    if result.columns.len() != 1
        || result.columns[0]
            != (ColumnMeta::Computed {
                name: "QUERY PLAN".to_string(),
                ty: Some(ColumnType::Text),
            })
    {
        return Err(ResponseEncodeError);
    }

    let capacity_hint = 16
        + result
            .rows
            .len()
            .saturating_mul(48 /* 行文字列の目安長 + 区切り文字 */);
    let mut out = String::with_capacity(capacity_hint);
    out.push_str("{\"explain\":[");
    for (i, row) in result.rows.iter().enumerate() {
        if row.cells.len() != 1 {
            return Err(ResponseEncodeError);
        }
        let Cell::Text(text) = &row.cells[0] else {
            return Err(ResponseEncodeError);
        };
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        escape_json_string_into(&mut out, text);
        out.push('"');
    }
    out.push_str("]}");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::catalog::ColumnType;
    use engine::json::{parse_json, JsonValue};
    use engine::sql::exec::ResultRow;
    use std::collections::BTreeMap;

    fn row(cells: Vec<Cell>) -> ResultRow {
        ResultRow {
            id: 1,
            score: 0.0,
            cells,
        }
    }

    /// トップレベルを `{"columns": Array, "rows": Array, "row_count": Number}`
    /// として構造的に解析するテスト専用ヘルパー。`unwrap`/`expect` はテスト
    /// コードでは許容する（`error_body.rs` の tests と同じ方針）。
    fn parse_top(body: &str) -> BTreeMap<String, JsonValue> {
        let parsed = parse_json(body).expect("valid JSON");
        let JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        top
    }

    fn all_column_kinds() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "lang".to_string(),
                ty: ColumnType::Text,
            },
            ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: ColumnType::Vector(2),
            },
            ColumnMeta::Computed {
                name: "n".to_string(),
                ty: Some(ColumnType::Double),
            },
        ]
    }

    #[test]
    fn round_trips_all_column_and_cell_kinds() {
        let result = QueryResult {
            columns: all_column_kinds(),
            rows: vec![row(vec![
                Cell::Integer(42),
                Cell::Text("ja".to_string()),
                Cell::Vector(vec![1.0, 2.5]),
                Cell::Float(3.5),
            ])],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);

        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        assert_eq!(columns.len(), 4);
        let names: Vec<&str> = columns
            .iter()
            .map(|c| {
                let JsonValue::Object(obj) = c else {
                    panic!("column must be an object");
                };
                let JsonValue::String(name) = &obj["name"] else {
                    panic!("name must be a string");
                };
                name.as_str()
            })
            .collect();
        assert_eq!(names, ["id", "lang", "embedding", "n"]);

        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        assert_eq!(rows.len(), 1);
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert_eq!(cells.len(), 4);
        assert!(matches!(&cells[0], JsonValue::Number(n) if n.as_f64() == 42.0));
        assert!(matches!(&cells[1], JsonValue::String(s) if s == "ja"));
        let JsonValue::Array(vec_cell) = &cells[2] else {
            panic!("vector cell must be an array");
        };
        assert_eq!(vec_cell.len(), 2);
        assert!(matches!(&cells[3], JsonValue::Number(n) if n.as_f64() == 3.5));

        assert!(matches!(&top["row_count"], JsonValue::Number(n) if n.as_f64() == 1.0));
    }

    /// `RowDescription`（SQL wire。Issue #895 の担当）が公告する OID とは
    /// **独立**に、NoSQL `columns[].type`（Issue #896・NOSQL-17）が列名ごとに
    /// 期待どおりの型名を返すことを固定する（両者は意図的に異なりうる。
    /// `nosql_type_name` のモジュール doc 参照）。
    #[test]
    fn column_type_returns_expected_nosql_type_name_for_every_column_kind() {
        let expected: [(&str, &str); 4] = [
            ("id", "numeric"),
            ("lang", "text"),
            ("embedding", "vector"),
            ("n", "text"),
        ];
        let columns = all_column_kinds();
        assert_eq!(
            expected.len(),
            columns.len(),
            "期待表の長さが ColumnMeta 種別数と一致すること"
        );

        // `RowDescription`（SQL wire。Issue #895 の担当）が独立に自己完結して
        // OID → 型名を解決できること（本モジュールの `nosql_type_name` とは
        // 非干渉であることの確認。`WireType::from_oid` を経由する唯一の
        // 呼び出し元がここであるため到達性も兼ねて固定する）。
        let row_description =
            crate::result_encoder::encode_row_description(&columns).expect("encode");
        let mut cursor = 1 + 4 + 2; // 'T' + length + field_count
        for _ in 0..columns.len() {
            let name_end = row_description[cursor..]
                .iter()
                .position(|&b| b == 0)
                .expect("NUL terminator");
            cursor += name_end + 1 + 4 + 2; // name + NUL + table_oid + attnum
            let oid_bytes: [u8; 4] = row_description[cursor..cursor + 4]
                .try_into()
                .expect("4 bytes");
            let oid = i32::from_be_bytes(oid_bytes);
            assert!(
                crate::result_encoder::WireType::from_oid(oid).is_some(),
                "RowDescription must announce a known OID: {oid}"
            );
            cursor += 4 + 2 + 4 + 2; // type_oid + typlen + typmod + format
        }

        let result = QueryResult {
            columns,
            rows: Vec::new(),
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(json_columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        for (i, (name, expected_type_name)) in expected.iter().enumerate() {
            let JsonValue::Object(col_obj) = &json_columns[i] else {
                panic!("column must be an object");
            };
            let JsonValue::String(actual_name) = &col_obj["name"] else {
                panic!("name must be a string");
            };
            assert_eq!(actual_name, name);
            let JsonValue::String(actual_type) = &col_obj["type"] else {
                panic!("type must be a string");
            };
            assert_eq!(actual_type, expected_type_name, "column={name}");
        }
    }

    /// `INTEGER`／`BIGINT` 列（Issue #881・TABLE-13・TASK-196）が `RowDescription`
    /// の OID と JSON `columns[].type` の双方で `int4`（OID 23）／`int8`（OID 20）
    /// として一致公告されることを固定する（Issue #903 レビュー指摘: 一律 `text`
    /// （OID 25）へ写像すると psql・ドライバ・ORM・JSON クライアントが整数列を
    /// 文字列として扱ってしまうため是正）。
    #[test]
    fn integer_and_bigint_columns_announce_nosql_type_names() {
        let columns = vec![
            ColumnMeta::Scalar {
                name: "n".to_string(),
                ty: ColumnType::Integer,
            },
            ColumnMeta::Scalar {
                name: "b".to_string(),
                ty: ColumnType::BigInt,
            },
        ];
        let expected: [(&str, &str); 2] = [("n", "integer"), ("b", "bigint")];

        let result = QueryResult {
            columns,
            rows: Vec::new(),
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(json_columns) = &top["columns"] else {
            panic!("columns must be an array");
        };

        for (i, (name, expected_type_name)) in expected.iter().enumerate() {
            let JsonValue::Object(col_obj) = &json_columns[i] else {
                panic!("column must be an object");
            };
            let JsonValue::String(actual_name) = &col_obj["name"] else {
                panic!("name must be a string");
            };
            assert_eq!(actual_name, name);
            let JsonValue::String(actual_type) = &col_obj["type"] else {
                panic!("type must be a string");
            };
            assert_eq!(actual_type, expected_type_name, "column={name}");
        }
    }

    /// 新規スカラー型（BOOLEAN／REAL／DOUBLE PRECISION／DATE／TIMESTAMP／
    /// BYTEA／UUID／JSON／JSONB。WIRE-13・TASK-200・Issue #895）が
    /// `RowDescription` の OID と JSON `columns[].type` の双方で
    /// 一致公告されることを固定する（NOSQL-17 の意図どおり、JSON 側の
    /// 型名も `"text"` から各専用型名へ変わる）。
    #[test]
    fn new_scalar_type_columns_announce_builtin_oid_and_json_type() {
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
        // OID（第 2 要素）は SQL wire `RowDescription` 側 `column_wire_type`
        // （Issue #895）の値。NoSQL 応答 `columns[].type`（第 3 要素）は
        // Issue #896 以降 `nosql_type_name` が独立の対応表として返す値で
        // あり、両者は列型によって意図的に異なる（`flag`: OID は
        // `bool`（16）のまま、NoSQL 型名は `boolean` 等。モジュール doc
        // 「独立の対応表」参照）。
        let expected: [(&str, i32, &str); 9] = [
            ("flag", 16, "boolean"),
            ("r", 700, "real"),
            ("d", 701, "double precision"),
            ("dt", 1082, "date"),
            ("ts", 1114, "timestamp"),
            ("blob", 17, "bytea"),
            ("uid", 2950, "uuid"),
            ("doc", 114, "json"),
            ("docb", 3802, "jsonb"),
        ];

        let row_description =
            crate::result_encoder::encode_row_description(&columns).expect("encode");
        let result = QueryResult {
            columns,
            rows: Vec::new(),
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(json_columns) = &top["columns"] else {
            panic!("columns must be an array");
        };

        let mut cursor = 1 + 4 + 2; // 'T' + length + field_count
        for (i, (name, expected_oid, expected_nosql_type_name)) in expected.iter().enumerate() {
            let name_end = row_description[cursor..]
                .iter()
                .position(|&b| b == 0)
                .expect("NUL terminator");
            let actual_name = std::str::from_utf8(&row_description[cursor..cursor + name_end])
                .expect("utf8 name");
            assert_eq!(&actual_name, name);
            cursor += name_end + 1;
            cursor += 4 + 2; // table_oid + attnum
            let oid_bytes: [u8; 4] = row_description[cursor..cursor + 4]
                .try_into()
                .expect("4 bytes");
            let oid = i32::from_be_bytes(oid_bytes);
            assert_eq!(oid, *expected_oid, "column={name}");
            cursor += 4; // type_oid
            cursor += 2 + 4 + 2; // typlen + typmod + format

            let JsonValue::Object(col_obj) = &json_columns[i] else {
                panic!("column must be an object");
            };
            let JsonValue::String(actual_type) = &col_obj["type"] else {
                panic!("type must be a string");
            };
            assert_eq!(actual_type, expected_nosql_type_name, "column={name}");
        }
    }

    /// Issue #1193: 配列セルのネイティブ JSON 表現（NULL 要素は `null`・新しい要素型・
    /// `columns[].type` の型名）。
    #[test]
    fn array_cells_render_native_json_with_null_elements_and_element_type_names() {
        use engine::catalog::{ArrayElemType, ArrayType};
        use engine::row_codec::ArrayValue as V;
        let uuid = engine::uuid::Uuid::from_bytes([0x11; 16]);
        let cases: Vec<(ArrayElemType, &str, V, &str)> = vec![
            (
                ArrayElemType::Text,
                "text[]",
                V::Text(vec![Some("a\"b".to_string()), None]),
                r#"["a\"b",null]"#,
            ),
            (
                ArrayElemType::Bool,
                "boolean[]",
                V::Bool(vec![None, Some(true)]),
                "[null,true]",
            ),
            (
                ArrayElemType::Integer,
                "integer[]",
                V::Integer(vec![Some(1), None, Some(-3)]),
                "[1,null,-3]",
            ),
            (
                ArrayElemType::BigInt,
                "bigint[]",
                V::BigInt(vec![Some(7), Some(9_007_199_254_740_993)]),
                r#"[7,"9007199254740993"]"#,
            ),
            (
                ArrayElemType::Real,
                "real[]",
                V::Real(vec![Some(1.5), None]),
                "[1.5,null]",
            ),
            (
                ArrayElemType::Double,
                "double precision[]",
                V::Double(vec![Some(2.25)]),
                "[2.25]",
            ),
            (
                ArrayElemType::Date,
                "date[]",
                V::Date(vec![Some(1), None]),
                r#"["1970-01-02",null]"#,
            ),
            (
                ArrayElemType::Timestamp,
                "timestamp[]",
                V::Timestamp(vec![Some(1_000_000)]),
                r#"["1970-01-01 00:00:01"]"#,
            ),
            (
                ArrayElemType::Uuid,
                "uuid[]",
                V::Uuid(vec![Some(uuid), None]),
                r#"["11111111-1111-1111-1111-111111111111",null]"#,
            ),
        ];
        for (elem, type_name, value, expected_cell) in cases {
            let result = QueryResult {
                columns: vec![ColumnMeta::Scalar {
                    name: "v".to_string(),
                    ty: ColumnType::Array(ArrayType::new(elem, 4).expect("array ty")),
                }],
                rows: vec![row(vec![Cell::Array(value)])],
            };
            let body = encode(&result).expect("encode");
            assert!(
                body.contains(&format!(r#""type":"{type_name}""#)),
                "type name {type_name} in {body}"
            );
            assert!(
                body.contains(&format!(r#""rows":[[{expected_cell}]]"#)),
                "cell {expected_cell} in {body}"
            );
        }
        // 非有限の REAL／DOUBLE 要素は fail-closed（JSON で表現できない）。
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "v".to_string(),
                ty: ColumnType::Array(ArrayType::new(ArrayElemType::Double, 4).expect("array ty")),
            }],
            rows: vec![row(vec![Cell::Array(V::Double(vec![Some(f64::NAN)]))])],
        };
        assert!(encode(&result).is_err());
    }

    #[test]
    fn row_count_equals_rows_len_for_empty_single_and_multiple_rows() {
        for n in [0usize, 1, 3] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Id],
                rows: (0..n as u64)
                    .map(|id| ResultRow {
                        id,
                        score: 0.0,
                        cells: vec![Cell::Integer(id)],
                    })
                    .collect(),
            };
            let body = encode(&result).expect("encode");
            let top = parse_top(&body);
            assert!(
                matches!(&top["row_count"], JsonValue::Number(v) if v.as_f64() == n as f64),
                "n={n}"
            );
            let JsonValue::Array(rows) = &top["rows"] else {
                panic!("rows must be an array");
            };
            assert_eq!(rows.len(), n);
        }
    }

    #[test]
    fn zero_column_result_has_empty_columns_and_empty_row_arrays() {
        let result = QueryResult {
            columns: Vec::new(),
            rows: vec![row(Vec::new())],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        assert!(columns.is_empty());
        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        assert_eq!(rows.len(), 1);
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert!(cells.is_empty());
    }

    #[test]
    fn golden_string_is_fixed() {
        let result = QueryResult {
            columns: vec![
                ColumnMeta::Id,
                ColumnMeta::Scalar {
                    name: "lang".to_string(),
                    ty: ColumnType::Text,
                },
            ],
            rows: vec![row(vec![Cell::Integer(1), Cell::Text("ja".to_string())])],
        };
        let body = encode(&result).expect("encode");
        assert_eq!(
            body,
            "{\"columns\":[{\"name\":\"id\",\"type\":\"numeric\"},\
{\"name\":\"lang\",\"type\":\"text\"}],\"rows\":[[1,\"ja\"]],\"row_count\":1}"
        );
    }

    /// NUMERIC 列（TABLE-13〔検討中〕・TASK-197、Issue #885）の JSON 出力は
    /// 正規テキスト表現（`Decimal::Display`）を bare な JSON number として書く
    /// （D8）。先頭ゼロ・指数表記・`-0` を持たないため JSON 文法として妥当。
    #[test]
    fn numeric_cell_encodes_as_bare_json_number() {
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "price".to_string(),
                ty: ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
            }],
            rows: vec![
                row(vec![Cell::Numeric(
                    engine::numeric::Decimal::from_parts(150, 2).expect("valid scale"),
                )]),
                row(vec![Cell::Numeric(
                    engine::numeric::Decimal::from_parts(-150, 2).expect("valid scale"),
                )]),
                row(vec![Cell::Null]),
            ],
        };
        let body = encode(&result).expect("encode");
        assert_eq!(
            body,
            "{\"columns\":[{\"name\":\"price\",\"type\":\"numeric\"}],\
\"rows\":[[1.50],[-1.50],[null]],\"row_count\":3}"
        );
    }

    /// UUID 列（TABLE-13〔検討中〕・TASK-197、Issue #887）の JSON 出力は
    /// 正規テキスト表現（小文字 `8-4-4-4-12`）を JSON string として書く（U4）。
    #[test]
    fn uuid_cell_encodes_as_json_string() {
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "ext_id".to_string(),
                ty: ColumnType::Uuid,
            }],
            rows: vec![
                row(vec![Cell::Uuid(
                    engine::uuid::parse_uuid_text("12345678-9abc-def0-1234-56789abcdef0")
                        .expect("valid uuid literal"),
                )]),
                row(vec![Cell::Null]),
            ],
        };
        let body = encode(&result).expect("encode");
        // `type` は WIRE-13・TASK-200・Issue #895 で `text`（OID 25）から
        // 専用 OID `uuid`（OID 2950）へ変わった（値表現〔JSON string〕は不変）。
        assert_eq!(
            body,
            "{\"columns\":[{\"name\":\"ext_id\",\"type\":\"uuid\"}],\
\"rows\":[[\"12345678-9abc-def0-1234-56789abcdef0\"],[null]],\"row_count\":2}"
        );
    }

    #[test]
    fn encode_is_deterministic() {
        let result = QueryResult {
            columns: all_column_kinds(),
            rows: vec![row(vec![
                Cell::Null,
                Cell::Text("x".to_string()),
                Cell::Vector(vec![1.0]),
                Cell::Bool(true),
            ])],
        };
        assert_eq!(encode(&result).unwrap(), encode(&result).unwrap());
    }

    /// U+0000〜U+001F 全走査＋`"`／`\` を含む Text セルと列名の両方を
    /// エスケープし、出力に 0x20 未満のバイトが無いこと・往復一致を確認する。
    #[test]
    fn escapes_control_chars_in_text_cell_and_column_name() {
        let mut message = String::new();
        for cp in 0u32..=0x1F {
            let ch = char::from_u32(cp).expect("valid control char codepoint");
            message.push(ch);
        }
        message.push('"');
        message.push('\\');

        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: message.clone(),
                ty: ColumnType::Text,
            }],
            rows: vec![row(vec![Cell::Text(message.clone())])],
        };
        let body = encode(&result).expect("encode");
        assert!(
            body.bytes().all(|b| b >= 0x20),
            "body must not contain raw bytes below 0x20: {body:?}"
        );

        let top = parse_top(&body);
        let JsonValue::Array(columns) = &top["columns"] else {
            panic!("columns must be an array");
        };
        let JsonValue::Object(col_obj) = &columns[0] else {
            panic!("column must be an object");
        };
        assert_eq!(col_obj["name"], JsonValue::String(message.clone()));

        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        assert_eq!(cells[0], JsonValue::String(message));
    }

    #[test]
    fn non_finite_float_cell_is_rejected() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Computed {
                    name: "n".to_string(),
                    ty: Some(ColumnType::Double),
                }],
                rows: vec![row(vec![Cell::Float(f)])],
            };
            let err = encode(&result).expect_err("non-finite float must be rejected");
            assert_eq!(
                ClassifiedError::error_class(&err),
                ErrorClass::InternalError
            );
        }
    }

    #[test]
    fn non_finite_vector_element_is_rejected() {
        for f in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let result = QueryResult {
                columns: vec![ColumnMeta::Scalar {
                    name: "embedding".to_string(),
                    ty: ColumnType::Vector(1),
                }],
                rows: vec![row(vec![Cell::Vector(vec![f])])],
            };
            let err = encode(&result).expect_err("non-finite vector element must be rejected");
            assert_eq!(
                ClassifiedError::error_class(&err),
                ErrorClass::InternalError
            );
        }
    }

    #[test]
    fn u64_max_integer_cell_has_no_precision_loss_in_decimal_text() {
        // `engine::json::JsonNumber::PosInt` は `u64::MAX` を無損失に表現できる
        // ため（Issue #823 レビュー指摘対応）`parse_json` 経由の往復でも精度は
        // 失われないが、本テストは `encode` が生成する 10 進テキスト自体が
        // `u64::MAX` と一致することを検証する（JSON パーサの実装詳細に
        // 依存しない、応答文字列そのものに対する直接検証）。
        let result = QueryResult {
            columns: vec![ColumnMeta::Id],
            rows: vec![row(vec![Cell::Integer(u64::MAX)])],
        };
        let body = encode(&result).expect("encode");
        assert!(body.contains(",\"row_count\":1}"));
        assert!(body.contains(&u64::MAX.to_string()));
    }

    /// `BIGINT`（`Cell::SignedInteger`）が `2^53-1` を超える場合は JSON 文字列
    /// で送出し、以下は bare な JSON number のまま送出することを固定する
    /// （Issue #896。JS 系クライアントの `Number.MAX_SAFE_INTEGER` 越え精度
    /// 誤解の防止）。
    #[test]
    fn signed_integer_cell_beyond_max_safe_integer_is_stringified() {
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "b".to_string(),
                ty: ColumnType::BigInt,
            }],
            rows: vec![
                row(vec![Cell::SignedInteger(MAX_SAFE_INTEGER)]),
                row(vec![Cell::SignedInteger(MAX_SAFE_INTEGER + 1)]),
                row(vec![Cell::SignedInteger(-(MAX_SAFE_INTEGER + 1))]),
                row(vec![Cell::SignedInteger(i64::MIN)]),
            ],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let cell_of = |i: usize| -> &JsonValue {
            let JsonValue::Array(cells) = &rows[i] else {
                panic!("row must be an array");
            };
            &cells[0]
        };
        assert!(
            matches!(cell_of(0), JsonValue::Number(n) if n.as_f64() == MAX_SAFE_INTEGER as f64)
        );
        assert_eq!(
            cell_of(1),
            &JsonValue::String((MAX_SAFE_INTEGER + 1).to_string())
        );
        assert_eq!(
            cell_of(2),
            &JsonValue::String((-(MAX_SAFE_INTEGER + 1)).to_string())
        );
        assert_eq!(cell_of(3), &JsonValue::String(i64::MIN.to_string()));
    }

    #[test]
    fn vector_cell_element_text_matches_result_encoder_bracketed_form() {
        let values = vec![1.0f32, 2.5, -3.0];
        let result = QueryResult {
            columns: vec![ColumnMeta::Scalar {
                name: "embedding".to_string(),
                ty: ColumnType::Vector(3),
            }],
            rows: vec![row(vec![Cell::Vector(values.clone())])],
        };
        let body = encode(&result).expect("encode");
        let top = parse_top(&body);
        let JsonValue::Array(rows) = &top["rows"] else {
            panic!("rows must be an array");
        };
        let JsonValue::Array(cells) = &rows[0] else {
            panic!("row must be an array");
        };
        let JsonValue::Array(vec_cell) = &cells[0] else {
            panic!("vector cell must be an array");
        };
        let numbers: Vec<f64> = vec_cell
            .iter()
            .map(|v| match v {
                JsonValue::Number(n) => n.as_f64(),
                other => panic!("expected number, got {other:?}"),
            })
            .collect();
        let expected: Vec<f64> = values.iter().map(|f| *f as f64).collect();
        assert_eq!(numbers, expected);
    }
}
