//! 表データ（行）の書き出し（論理ダンプのデータ側。RECOVER-16・Issue #2161。親モジュール: [`super`]）。
//!
//! 責務境界: 読み取りスナップショット 1 つの上で、範囲内の表を走査して行を `COPY ... FROM STDIN`
//! 形式、または `INSERT` 文としてシンクへ 1 行ずつ渡す。値のテキスト表現は WIRE-13 の型ごとの表現に
//! 揃える（ベクトル・配列・JSON・日付時刻を含む）。表の定義（DDL）は [`super::object`] が担い、
//! 呼び出し側（#2162 の CLI）が `PreData` → データ → `PostData` の順に組み合わせる。
//!
//! # テナント境界（P0）
//!
//! 行ストアの物理キーは `(tenant_id, id)` で、指定テナントの行は 1 本の連続範囲になる。走査は
//! その範囲に限り、他テナントの行は読み出しもしない（構造的な絞り込み）。範囲内でも復号した行の
//! テナントがキーと食い違えば失敗させる。テナントの無い範囲（[`SchemaDdlScope::shared_only`]）は
//! 行がすべてテナント所有のため、何も出さない。全テナントのダンプは、呼び出し側がテナントごとに
//! 範囲を変えて続けて呼ぶ。
//!
//! # 書き出せないものは失敗させる（fail-closed）
//!
//! DDL 側と同じく、SQL で再投入できない表・行は省かず [`SchemaDdlError::Unsupported`] で全体を
//! 失敗させる（暗黙 `id` の表・可視性が `Private` でない行・`INSERT` 形式で書けない列など）。
//! エラー文言は固定文字列で、表名・テナント名・行の内容を含まない。

use std::ops::Bound;

use redb::ReadableDatabase;

use super::super::namespace::schema_names_for_tenant_in_txn;
use super::super::{
    get_table_schema_in_txn_impl, user_rows_physical_name, user_rows_table_def, CatalogError,
    ColumnDefault, ColumnType, IdentityKind, RowIdentity, Storage, TableSchema,
};
use super::quote::{push_ident, Namer};
use super::{
    ensure_all_tables_classified, object, unsupported, Ctx, SchemaDdlError, SchemaDdlScope,
};
use crate::row_codec::{decode_scalar_columns, Value};
use crate::storage::rowid::RowId;
use crate::storage::{decode_row_for_key, Visibility};

/// データの書き出し形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableDataFormat {
    /// `COPY ... FROM STDIN`（text 形式）。ただし COPY が受理しない列型（`HALFVEC`・`INT8VEC`・
    /// `SPARSEVEC`）を持つ表は、その表だけ `INSERT` 文で書き出す。
    Copy,
    /// 1 行 1 文の `INSERT`。`GENERATED ALWAYS` 列を持つ表は書き込めないため失敗させる。
    Insert,
}

/// シンクへ渡す断片の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableDataPart {
    /// `COPY <表> (<列>, ...) FROM STDIN`（`;` なし）。続く [`Self::CopyRow`] の前に 1 回出る。
    CopyHeader,
    /// COPY text 形式のデータ行 1 件（エスケープ済み・末尾の改行なし）。
    CopyRow,
    /// COPY の終端行（`\.`）。
    CopyEnd,
    /// `INSERT INTO <表> (<列>, ...) VALUES (...)` 1 文（`;` なし）。
    Insert,
}

/// シンクへ渡すデータ断片 1 件。
///
/// `text` は行の内容を含むため、`Debug` は伏せる（ログに出さない）。呼び出し側は
/// [`TableDataPart::CopyHeader`]・[`TableDataPart::Insert`] の末尾に `;` と改行を、
/// [`TableDataPart::CopyRow`]・[`TableDataPart::CopyEnd`] の末尾に改行を付けて書く。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TableDataItem<'a> {
    /// 行を所有するテナント。復元時はこのテナントの文脈で実行する。
    pub tenant: &'a str,
    /// 断片の種別。
    pub part: TableDataPart,
    /// 断片の本文。
    pub text: &'a str,
}

impl std::fmt::Debug for TableDataItem<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableDataItem")
            .field("part", &self.part)
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .finish()
    }
}

/// 書き出しの件数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableDataSummary {
    /// 1 行以上を書き出した表の数。
    pub tables: u64,
    /// 書き出した行の総数。
    pub rows: u64,
}

/// 書き出しに使う形式（表ごとに [`TableDataFormat::Copy`] が `INSERT` へ落ちることがある）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EffectiveFormat {
    Copy,
    Insert,
}

/// COPY が受理しない列型か（`bind_copy_record` が拒否する型と対応させる）。
fn copy_unsupported(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::HalfVec(_) | ColumnType::Int8Vec(_) | ColumnType::SparseVec(_)
    )
}

/// `INSERT` で明示値を書けない列（`GENERATED ALWAYS`。`OVERRIDING` を受理しないため）。
fn is_generated_always(default: Option<&ColumnDefault>) -> bool {
    matches!(
        default,
        Some(ColumnDefault::Identity {
            kind: IdentityKind::Always,
            ..
        })
    )
}

/// COPY text 形式のフィールドへエスケープして追記する。本リポの COPY 復号器が受理する
/// エスケープ（`\\`・`\t`・`\n`・`\r`・`\b`・`\f`・`\v`）だけを使い、それ以外は素通しにする。
fn push_copy_field(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            other => out.push(other),
        }
    }
}

/// `[a,b,c]` 形（ベクトル系列の WIRE-13 テキスト表現）を組み立てる。
fn bracket_list<T>(items: impl Iterator<Item = T>, mut render: impl FnMut(T) -> String) -> String {
    let mut s = String::from("[");
    for (i, it) in items.enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&render(it));
    }
    s.push(']');
    s
}

/// 値のテキスト表現（WIRE-13）。`Null` は `None`。網羅 match で書き、型が増えたときに
/// コンパイルエラーで書き出しの対応漏れを検出する（暗号化列のように平文で出せない型が増えた場合も、
/// ここで明示的に `Unsupported` にすること）。
fn value_text<E>(v: &Value) -> Result<Option<String>, SchemaDdlError<E>> {
    let tz = crate::datetime::SessionTimeZone::utc();
    Ok(Some(match v {
        Value::Null => return Ok(None),
        Value::Text(s) | Value::Json(s) | Value::Enum(s) => s.clone(),
        Value::Vector(xs) => bracket_list(xs.iter(), |f| f.to_string()),
        Value::Integer(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::Real(f) => crate::scalar_float::format_real(*f),
        Value::Double(f) => crate::scalar_float::format_double(*f),
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Date(d) => crate::datetime::format_date(*d),
        Value::Timestamp(t) => crate::datetime::format_timestamp(*t),
        Value::Time(t) => crate::datetime::format_time(*t),
        // セッションの TimeZone に依存しないよう UTC の壁時計＋ offset（`+00`）で出す。
        Value::TimestampTz(t) => crate::datetime::format_timestamptz(*t, &tz),
        Value::Interval(i) => crate::datetime::format_interval(i),
        Value::Bit(b) => b.format_text(),
        Value::HalfVec(bits) => {
            bracket_list(bits.iter(), |b| crate::f16::f16_bits_to_f32(*b).to_string())
        }
        Value::Int8Vec(xs) => bracket_list(xs.iter(), |x| x.to_string()),
        Value::SparseVec(sv) => sv.format_text(),
        Value::Array(a) => a.to_pg_text(),
        Value::Bytes(b) => crate::bytea::format_hex_text(b),
        Value::Numeric(d) => d.to_string(),
        Value::Uuid(u) => u.to_string(),
    }))
}

/// `INSERT` の値位置へ書くリテラル。本リポの `INSERT` は数値列に文字列リテラルを受理しない
/// （暗黙変換をしない設計）ため、数値・真偽値は裸で、それ以外は引用符付き文字列で書く。
fn insert_literal<E>(v: &Value) -> Result<String, SchemaDdlError<E>> {
    let Some(text) = value_text::<E>(v)? else {
        return Ok("NULL".to_string());
    };
    Ok(match v {
        Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Numeric(_) => text,
        Value::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).to_string(),
        Value::Null
        | Value::Text(_)
        | Value::Json(_)
        | Value::Enum(_)
        | Value::Vector(_)
        | Value::Date(_)
        | Value::Timestamp(_)
        | Value::Time(_)
        | Value::TimestampTz(_)
        | Value::Interval(_)
        | Value::Bit(_)
        | Value::HalfVec(_)
        | Value::Int8Vec(_)
        | Value::SparseVec(_)
        | Value::Array(_)
        | Value::Bytes(_)
        | Value::Uuid(_) => super::quote::literal(&text)?,
    })
}

/// 行の列値を `schema.columns` の順に復号する。`VECTOR` 列は行の embedding から補う
/// （空は NULL。nullable な VECTOR 列の NULL の保存形）。
fn row_values<E>(
    schema: &TableSchema,
    metadata: &[u8],
    embedding: Vec<f32>,
) -> Result<Vec<Value>, SchemaDdlError<E>> {
    let corrupt =
        || SchemaDdlError::Catalog(CatalogError::CorruptSchema("row is unreadable".into()));
    let mut values = decode_scalar_columns(schema, metadata).map_err(|_| corrupt())?;
    if values.len() != schema.columns.len() {
        return Err(corrupt());
    }
    if let Some(pos) = schema.columns.iter().position(|c| c.ty.is_vector()) {
        let slot = values.get_mut(pos).ok_or_else(corrupt)?;
        *slot = if embedding.is_empty() {
            Value::Null
        } else {
            Value::Vector(embedding)
        };
    } else if !embedding.is_empty() {
        return Err(corrupt());
    }
    Ok(values)
}

/// 出力先の断片シンクと件数。
struct DataEmitter<'a, E> {
    sink: &'a mut dyn FnMut(&TableDataItem<'_>) -> Result<(), E>,
    tenant: &'a str,
}

impl<E> DataEmitter<'_, E> {
    fn emit(&mut self, part: TableDataPart, text: &str) -> Result<(), SchemaDdlError<E>> {
        (self.sink)(&TableDataItem {
            tenant: self.tenant,
            part,
            text,
        })
        .map_err(SchemaDdlError::Sink)
    }
}

/// 範囲内の全表の行を書き出す。`ctx` の読み取りスナップショット上で動く。
fn export_rows<E>(
    ctx: &Ctx<'_>,
    tenant: &str,
    format: TableDataFormat,
    sink: &mut dyn FnMut(&TableDataItem<'_>) -> Result<(), E>,
) -> Result<TableDataSummary, SchemaDdlError<E>> {
    let mut out = DataEmitter { sink, tenant };
    let mut summary = TableDataSummary::default();
    for key in object::data_table_keys::<E>(ctx)? {
        let schema = get_table_schema_in_txn_impl(ctx.rt, &key, true)?;
        // DDL 側が再作成できない表は、行だけ書き出しても復元できない。
        match schema.row_identity() {
            RowIdentity::Allocated => {}
            RowIdentity::LegacyId | RowIdentity::MigratedId => {
                return Err(unsupported(
                    "table data",
                    "table with an implicit id row identity cannot be restored by SQL",
                ));
            }
        }
        if schema.columns.is_empty() {
            return Err(unsupported("table data", "table without live columns"));
        }
        let effective = match format {
            TableDataFormat::Copy if schema.columns.iter().any(|c| copy_unsupported(&c.ty)) => {
                EffectiveFormat::Insert
            }
            TableDataFormat::Copy => EffectiveFormat::Copy,
            TableDataFormat::Insert => EffectiveFormat::Insert,
        };
        if effective == EffectiveFormat::Insert
            && schema
                .columns
                .iter()
                .any(|c| is_generated_always(c.default_expr.as_ref()))
        {
            return Err(unsupported(
                "table data",
                "generated always column cannot be written by INSERT",
            ));
        }
        let table_ref = ctx.namer.qualified_required::<E>(&key)?;
        let mut column_list = String::from("(");
        for (i, c) in schema.columns.iter().enumerate() {
            if i > 0 {
                column_list.push_str(", ");
            }
            push_ident(&mut column_list, &c.name)?;
        }
        column_list.push(')');
        let rows = export_table_rows(
            ctx,
            tenant,
            &key,
            &schema,
            effective,
            &table_ref,
            &column_list,
            &mut out,
        )?;
        if rows > 0 {
            summary.tables = summary.tables.saturating_add(1);
            summary.rows = summary.rows.saturating_add(rows);
        }
    }
    Ok(summary)
}

/// 1 つの表について、指定テナントの行を書き出す。書き出した行数を返す。
#[allow(clippy::too_many_arguments)]
fn export_table_rows<E>(
    ctx: &Ctx<'_>,
    tenant: &str,
    key: &str,
    schema: &TableSchema,
    effective: EffectiveFormat,
    table_ref: &str,
    column_list: &str,
    out: &mut DataEmitter<'_, E>,
) -> Result<u64, SchemaDdlError<E>> {
    let physical = user_rows_physical_name(key);
    let table = match ctx.rt.open_table(user_rows_table_def(&physical)) {
        Ok(t) => t,
        // 行を 1 件も入れていない表は行テーブルが未作成。
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    // 指定テナントの行だけの連続範囲（`(tenant_id, id)` 昇順）。他テナントの行は読まない。
    let range = (
        Bound::Included((tenant, 0u64)),
        Bound::Included((tenant, u64::MAX)),
    );
    let mut rows: u64 = 0;
    // 1 行ごとに使い回すバッファ（メモリ使用量は表の大きさに比例させない）。
    let mut line = String::new();
    for entry in table
        .range::<(&str, u64)>(range)
        .map_err(CatalogError::from)?
    {
        let (k, v) = entry.map_err(CatalogError::from)?;
        let (row_tenant, id) = k.value();
        let row =
            decode_row_for_key(row_tenant, RowId::from_legacy_id(id), v.value()).map_err(|_| {
                SchemaDdlError::Catalog(CatalogError::CorruptSchema("row is unreadable".into()))
            })?;
        if row.tenant_id != tenant {
            return Err(SchemaDdlError::Catalog(CatalogError::CorruptSchema(
                "row is unreadable".into(),
            )));
        }
        // SQL の `INSERT`／`COPY` が書く行は常に `Private`。再投入で可視性を変えないため、
        // 他の可視性の行は黙って `Private` へ落とさず失敗させる。
        if row.visibility != Visibility::Private {
            return Err(unsupported(
                "table data",
                "row with a visibility that SQL cannot restore",
            ));
        }
        let values = row_values::<E>(schema, &row.metadata, row.embedding)?;
        if rows == 0 && effective == EffectiveFormat::Copy {
            line.clear();
            line.push_str("COPY ");
            line.push_str(table_ref);
            line.push(' ');
            line.push_str(column_list);
            line.push_str(" FROM STDIN");
            out.emit(TableDataPart::CopyHeader, &line)?;
        }
        line.clear();
        let part = match effective {
            EffectiveFormat::Copy => {
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    match value_text::<E>(v)? {
                        Some(t) => push_copy_field(&mut line, &t),
                        None => line.push_str("\\N"),
                    }
                }
                TableDataPart::CopyRow
            }
            EffectiveFormat::Insert => {
                line.push_str("INSERT INTO ");
                line.push_str(table_ref);
                line.push(' ');
                line.push_str(column_list);
                line.push_str(" VALUES (");
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        line.push_str(", ");
                    }
                    line.push_str(&insert_literal::<E>(v)?);
                }
                line.push(')');
                TableDataPart::Insert
            }
        };
        // NUL は COPY・文字列リテラルのどちらでも運べない。
        if line.contains('\0') {
            return Err(unsupported("table data", "value contains a NUL character"));
        }
        out.emit(part, &line)?;
        rows = rows.saturating_add(1);
    }
    if rows > 0 && effective == EffectiveFormat::Copy {
        out.emit(TableDataPart::CopyEnd, "\\.")?;
    }
    Ok(rows)
}

impl Storage {
    /// 範囲内の表の行を `format` で `sink` へ 1 件ずつ渡す（RECOVER-16 (1)(2)）。
    ///
    /// 読み取りトランザクション 1 つの上で行を順に走査し、書き込みを止めず、メモリ使用量を表の大きさに
    /// 比例させない（行数に上限は設けない）。範囲にテナントが無い場合は何も出さない。行は指定テナントの
    /// ものだけで、他テナントの行・存在・件数は出力にもエラーにも現れない。書き出せないものがあれば
    /// [`SchemaDdlError::Unsupported`]（それまでに渡した断片は破棄すること）。
    ///
    /// 運用者の CLI（#2162）専用の入口で、テナントの認可は呼び出し側が担う。表の定義は
    /// [`Storage::export_schema_ddl`] が別のスナップショットで書くため、両者を 1 つの commit 点に
    /// 揃える場合は [`Storage::export_logical_dump`] を使う。
    pub fn export_table_data<E>(
        &self,
        scope: &SchemaDdlScope,
        format: TableDataFormat,
        mut sink: impl FnMut(&TableDataItem<'_>) -> Result<(), E>,
    ) -> Result<TableDataSummary, SchemaDdlError<E>> {
        let rt = self.db().begin_read().map_err(CatalogError::from)?;
        export_table_data_in(&rt, scope, format, &mut sink)
    }
}

/// 呼び出し側が開いた読み取りスナップショット上での [`Storage::export_table_data`] 本体。
pub(super) fn export_table_data_in<E>(
    rt: &redb::ReadTransaction,
    scope: &SchemaDdlScope,
    format: TableDataFormat,
    sink: &mut dyn FnMut(&TableDataItem<'_>) -> Result<(), E>,
) -> Result<TableDataSummary, SchemaDdlError<E>> {
    let Some(tenant) = scope.tenant() else {
        return Ok(TableDataSummary::default());
    };
    super::super::role::validate_tenant(tenant).map_err(SchemaDdlError::Catalog)?;
    ensure_all_tables_classified(rt)?;
    let namer = Namer {
        schemas: schema_names_for_tenant_in_txn(rt, tenant)?,
    };
    let ctx = Ctx { rt, scope, namer };
    export_rows(&ctx, tenant, format, sink)
}
