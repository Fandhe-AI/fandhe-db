//! 表・外部キー・索引・VIEW の DDL 生成（RECOVER-16 (1)。親モジュール: [`super`]）。
//!
//! 責務境界: カタログ上の定義（[`TableSchema`]・[`IndexDef`]・[`ViewDef`]）を、本リポの SQL 表層が
//! 受理する `CREATE TABLE`／`ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`／`CREATE INDEX`／
//! `CREATE VIEW` の文字列へ戻す。列型・既定値の描画は `ColumnType`／`ColumnDefault` を網羅 match し、
//! ワイルドカード腕を置かない（型が増えたときにコンパイルエラーで書き出しの対応漏れを検出するため）。
//! DDL へ戻せない形（採番方式の異なる表・内部キーを含む保存済み本文・名前の合わない暗黙 SEQUENCE
//! など）は [`super::unsupported`] で全体を失敗させる。

use super::super::sequence::{make_object_name, SequenceDef, SequenceOwner, SequenceParams};
use super::super::{
    ArrayElemType, CheckConstraint, ColumnDef, ColumnDefault, ColumnType, ForeignKeyDef,
    ForeignKeyDeferrability, ForeignKeyMatch, HnswOpClassFamily, IdentityKind, IndexDef, IndexKind,
    ReferentialAction, RowIdentity, TableSchema, TimeDefault, ViewCheckOption, ViewDef,
};
use super::quote::{ensure_no_internal_key, ident, literal, push_collation, push_ident};
use super::{unsupported, Ctx, SchemaDdlError};
use std::collections::BTreeMap;

/// 暗黙 SEQUENCE の索引（SEQUENCE の relation キー → 定義と所有者）。
pub(super) type SequenceMap = BTreeMap<String, (SequenceDef, Option<SequenceOwner>)>;

/// 列型を SQL の型名へ描画する。ENUM は型名を引用符付きで出す。
pub(super) fn render_column_type<E>(ty: &ColumnType) -> Result<String, SchemaDdlError<E>> {
    Ok(match ty {
        ColumnType::Text => "TEXT".to_string(),
        ColumnType::Varchar(None) => "VARCHAR".to_string(),
        ColumnType::Varchar(Some(n)) => format!("VARCHAR({n})"),
        ColumnType::Bpchar(n) => format!("CHAR({n})"),
        ColumnType::Vector(n) => format!("VECTOR({n})"),
        ColumnType::HalfVec(n) => format!("HALFVEC({n})"),
        ColumnType::Int8Vec(n) => format!("INT8VEC({n})"),
        ColumnType::SparseVec(n) => format!("SPARSEVEC({n})"),
        ColumnType::SmallInt => "SMALLINT".to_string(),
        ColumnType::Integer => "INTEGER".to_string(),
        ColumnType::BigInt => "BIGINT".to_string(),
        ColumnType::Real => "REAL".to_string(),
        ColumnType::Double => "DOUBLE PRECISION".to_string(),
        ColumnType::Boolean => "BOOLEAN".to_string(),
        ColumnType::Date => "DATE".to_string(),
        ColumnType::Timestamp => "TIMESTAMP".to_string(),
        ColumnType::Time => "TIME".to_string(),
        ColumnType::TimestampTz => "TIMESTAMPTZ".to_string(),
        ColumnType::Interval => "INTERVAL".to_string(),
        ColumnType::Bit(n) => format!("BIT({n})"),
        ColumnType::Bytea => "BYTEA".to_string(),
        ColumnType::Json => "JSON".to_string(),
        ColumnType::Jsonb => "JSONB".to_string(),
        ColumnType::Uuid => "UUID".to_string(),
        ColumnType::Numeric { precision, scale } => format!("NUMERIC({precision}, {scale})"),
        ColumnType::Enum(def) => ident(def.name())?,
        ColumnType::Array(a) => {
            let elem = match (a.elem(), a.enum_def()) {
                (ArrayElemType::Enum, Some(def)) => ident::<E>(def.name())?,
                (ArrayElemType::Enum, None) => {
                    return Err(unsupported("column type", "enum array without a type"));
                }
                (ArrayElemType::Text, _) => "TEXT".to_string(),
                (ArrayElemType::Bool, _) => "BOOLEAN".to_string(),
                (ArrayElemType::Integer, _) => "INTEGER".to_string(),
                (ArrayElemType::BigInt, _) => "BIGINT".to_string(),
                (ArrayElemType::Real, _) => "REAL".to_string(),
                (ArrayElemType::Double, _) => "DOUBLE PRECISION".to_string(),
                (ArrayElemType::Date, _) => "DATE".to_string(),
                (ArrayElemType::Timestamp, _) => "TIMESTAMP".to_string(),
                (ArrayElemType::Uuid, _) => "UUID".to_string(),
                (ArrayElemType::Numeric { precision, scale }, _) => {
                    format!("NUMERIC({precision}, {scale})")
                }
                (ArrayElemType::Bytea, _) => "BYTEA".to_string(),
                (ArrayElemType::Json, _) => "JSON".to_string(),
                (ArrayElemType::Jsonb, _) => "JSONB".to_string(),
            };
            // 要素数の上限 `[N]` は最初の次元にだけ付ける（構文上 `[N]` は全体で高々 1 個）。
            let mut s = format!("{elem}[{}]", a.max_len());
            for _ in 1..a.ndims() {
                s.push_str("[]");
            }
            s
        }
    })
}

/// 数値リテラルとして安全な文字だけで構成されているかを確かめて返す（SQL へ連結する前の検証）。
pub(super) fn safe_number<E>(text: &str) -> Result<&str, SchemaDdlError<E>> {
    let ok = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        // 文字種に加えて数値として読めることを確かめる（`--` のコメント開始などを含む形を通さない）。
        && !text.contains("--")
        && (text.parse::<i128>().is_ok() || text.parse::<f64>().is_ok_and(f64::is_finite));
    if ok {
        Ok(text)
    } else {
        Err(unsupported(
            "column default",
            "numeric default is not a plain number",
        ))
    }
}

/// 時刻値関数の DEFAULT を SQL の構文形へ描画する。
fn render_time_default(td: TimeDefault) -> String {
    let with_p = |name: &str, p: Option<u8>| match p {
        Some(p) => format!("{name}({p})"),
        None => name.to_string(),
    };
    match td {
        TimeDefault::Now => "now()".to_string(),
        TimeDefault::TransactionTimestamp => "transaction_timestamp()".to_string(),
        TimeDefault::CurrentTimestamp(p) => with_p("CURRENT_TIMESTAMP", p),
        TimeDefault::LocalTimestamp(p) => with_p("LOCALTIMESTAMP", p),
        TimeDefault::CurrentDate => "CURRENT_DATE".to_string(),
        TimeDefault::StatementTimestamp => "statement_timestamp()".to_string(),
        TimeDefault::ClockTimestamp => "clock_timestamp()".to_string(),
        TimeDefault::LocalTime(p) => with_p("LOCALTIME", p),
    }
}

/// 採番しない DEFAULT（定数・時刻値関数）を `DEFAULT <式>` の式部分へ描画する。採番する DEFAULT
/// （`nextval`・IDENTITY）は呼び出し側が別に扱う。
fn render_plain_default<E>(d: &ColumnDefault) -> Result<Option<String>, SchemaDdlError<E>> {
    Ok(match d {
        ColumnDefault::Text(s) => Some(literal(s)?),
        ColumnDefault::Number(n) => Some(safe_number(n)?.to_string()),
        ColumnDefault::Bool(true) => Some("TRUE".to_string()),
        ColumnDefault::Bool(false) => Some("FALSE".to_string()),
        ColumnDefault::CurrentTime(td) => Some(render_time_default(*td)),
        ColumnDefault::Sequence(_) | ColumnDefault::Identity { .. } => None,
    })
}

/// SERIAL 系の暗黙 SEQUENCE が取る既定の定義か（列型から決まる上限）。
fn is_serial_shape(ty: &ColumnType, p: &SequenceParams) -> Option<&'static str> {
    let (name, max) = match ty {
        ColumnType::SmallInt => ("SMALLSERIAL", i64::from(i16::MAX)),
        ColumnType::Integer => ("SERIAL", i64::from(i32::MAX)),
        ColumnType::BigInt => ("BIGSERIAL", i64::MAX),
        _ => return None,
    };
    (p.increment == 1 && p.min_value == 1 && p.max_value == max && p.start == 1 && !p.cycle)
        .then_some(name)
}

/// 暗黙 SEQUENCE の名前が、`CREATE TABLE` の再実行で付く名前と一致するか。再実行時に付く名前は
/// 表名と列名から決まり、元の名前が衝突回避の番号付きなど別の名前なら復元後に名前が変わるため、
/// 一致しない場合は書き出せない。
fn implicit_sequence_name_matches(table_key: &str, column: &str, seq_key: &str) -> bool {
    let strip = |k: &str| {
        k.split_once('.')
            .map_or_else(|| k.to_string(), |(_, n)| n.to_string())
    };
    let prefix = |k: &str| k.split_once('.').map(|(p, _)| p.to_string());
    let expected = make_object_name(&strip(table_key), column, "seq");
    prefix(table_key) == prefix(seq_key) && strip(seq_key) == expected
}

/// IDENTITY の `GENERATED` 句（SEQUENCE のオプションを明示する）を描画する。
fn render_identity(kind: IdentityKind, p: &SequenceParams) -> String {
    let which = match kind {
        IdentityKind::Always => "ALWAYS",
        IdentityKind::ByDefault => "BY DEFAULT",
    };
    format!(
        "GENERATED {which} AS IDENTITY (START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {})",
        p.start,
        p.increment,
        p.min_value,
        p.max_value,
        if p.cycle { "CYCLE" } else { "NO CYCLE" }
    )
}

/// 列定義 1 件を `CREATE TABLE` の列定義文字列へ描画する。
fn render_column<E>(
    ctx: &Ctx<'_>,
    table_key: &str,
    col: &ColumnDef,
    seqs: &SequenceMap,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::new();
    push_ident(&mut s, &col.name)?;
    s.push(' ');
    let mut not_null = !col.nullable;
    let mut tail = String::new();
    match &col.default_expr {
        Some(ColumnDefault::Sequence(seq)) => {
            // 所有される SEQUENCE は SERIAL 系でしか再現できない。所有されない SEQUENCE は
            // 別途 `CREATE SEQUENCE` を出してあるので、通常の `nextval` 既定値として描画する。
            match seqs.get(seq) {
                Some((def, Some(owner))) => {
                    if owner.table != table_key || owner.column != col.name {
                        return Err(unsupported(
                            "sequence",
                            "owned by a different column than the one using it",
                        ));
                    }
                    let name = is_serial_shape(&col.ty, &def.params).ok_or_else(|| {
                        unsupported("sequence", "owned sequence with non-default parameters")
                    })?;
                    if def.restart_value != def.params.start {
                        return Err(unsupported(
                            "sequence",
                            "owned sequence with a restart value",
                        ));
                    }
                    if col.nullable {
                        return Err(unsupported("sequence", "nullable serial column"));
                    }
                    if !implicit_sequence_name_matches(table_key, &col.name, seq) {
                        return Err(unsupported(
                            "sequence",
                            "owned sequence with a non-default name",
                        ));
                    }
                    s.push_str(name);
                    not_null = false;
                }
                Some((_, None)) => {
                    s.push_str(&render_column_type::<E>(&col.ty)?);
                    let arg = ctx.namer.sequence_arg::<E>(seq)?.ok_or_else(|| {
                        unsupported("sequence", "reference outside the dump scope")
                    })?;
                    tail.push_str(" DEFAULT nextval(");
                    tail.push_str(&literal::<E>(&arg)?);
                    tail.push(')');
                }
                None => {
                    return Err(unsupported(
                        "sequence",
                        "default refers to a missing sequence",
                    ))
                }
            }
        }
        Some(ColumnDefault::Identity { sequence, kind }) => {
            let (def, owner) = seqs
                .get(sequence)
                .ok_or_else(|| unsupported("sequence", "identity refers to a missing sequence"))?;
            let owned_here = owner
                .as_ref()
                .is_some_and(|o| o.table == table_key && o.column == col.name);
            if !owned_here {
                return Err(unsupported(
                    "sequence",
                    "identity sequence has a different owner",
                ));
            }
            if def.restart_value != def.params.start {
                return Err(unsupported(
                    "sequence",
                    "owned sequence with a restart value",
                ));
            }
            if !implicit_sequence_name_matches(table_key, &col.name, sequence) {
                return Err(unsupported(
                    "sequence",
                    "owned sequence with a non-default name",
                ));
            }
            s.push_str(&render_column_type::<E>(&col.ty)?);
            tail.push(' ');
            tail.push_str(&render_identity(*kind, &def.params));
        }
        Some(d) => {
            s.push_str(&render_column_type::<E>(&col.ty)?);
            if let Some(expr) = render_plain_default::<E>(d)? {
                tail.push_str(" DEFAULT ");
                tail.push_str(&expr);
            }
        }
        None => s.push_str(&render_column_type::<E>(&col.ty)?),
    }
    if let Some(c) = col.collation {
        s.push_str(" COLLATE ");
        push_collation(&mut s, c);
    }
    if not_null {
        s.push_str(" NOT NULL");
    }
    s.push_str(&tail);
    Ok(s)
}

/// 列名の並びを `("a", "b")` の形へ描画する。
fn render_column_list<E>(cols: &[String]) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::from("(");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        push_ident(&mut s, c)?;
    }
    s.push(')');
    Ok(s)
}

/// 表の `CREATE TABLE` 文を作る。FOREIGN KEY は含めない（データの後ろの別文）。
/// 一意索引（`CREATE UNIQUE INDEX`）が同名で登録した UNIQUE 制約は、索引の文が作り直すので
/// ここでは出さない（二重定義になるため）。
pub(super) fn render_create_table<E>(
    ctx: &Ctx<'_>,
    table_key: &str,
    schema: &TableSchema,
    seqs: &SequenceMap,
    unique_index_names: &[String],
) -> Result<String, SchemaDdlError<E>> {
    match schema.row_identity() {
        RowIdentity::Allocated => {}
        RowIdentity::LegacyId | RowIdentity::MigratedId => {
            return Err(unsupported(
                "table",
                "table with an implicit id row identity cannot be recreated by SQL",
            ));
        }
    }
    if schema.columns.is_empty() {
        return Err(unsupported("table", "table without live columns"));
    }
    let mut items: Vec<String> = Vec::new();
    for col in &schema.columns {
        items.push(render_column::<E>(ctx, table_key, col, seqs)?);
    }
    if let Some(pk) = schema.primary_key() {
        let mut s = String::new();
        if let Some(name) = explicit_primary_key_name(schema) {
            s.push_str("CONSTRAINT ");
            push_ident(&mut s, &name)?;
            s.push(' ');
        }
        s.push_str("PRIMARY KEY ");
        s.push_str(&render_column_list::<E>(pk)?);
        items.push(s);
    }
    for uc in schema.unique_constraints() {
        if unique_index_names.iter().any(|n| n == uc.name()) {
            continue;
        }
        let mut s = String::from("CONSTRAINT ");
        push_ident(&mut s, uc.name())?;
        s.push_str(" UNIQUE ");
        s.push_str(&render_column_list::<E>(uc.columns())?);
        items.push(s);
    }
    for ck in schema.checks() {
        items.push(render_check::<E>(ck)?);
    }
    let mut out = String::from("CREATE TABLE ");
    out.push_str(&ctx.namer.qualified_required::<E>(table_key)?);
    out.push_str(" (");
    out.push_str(&items.join(", "));
    out.push(')');
    Ok(out)
}

/// 導出名（`<table>_pkey`）と異なる明示名がある場合だけ返す。
fn explicit_primary_key_name(schema: &TableSchema) -> Option<String> {
    schema.primary_key_name.clone()
}

/// `CHECK` 制約 1 件を描画する。
fn render_check<E>(ck: &CheckConstraint) -> Result<String, SchemaDdlError<E>> {
    ensure_no_internal_key("check constraint", &ck.predicate_sql)?;
    let mut s = String::from("CONSTRAINT ");
    push_ident(&mut s, &ck.name)?;
    s.push_str(" CHECK (");
    s.push_str(&ck.predicate_sql);
    s.push(')');
    Ok(s)
}

fn render_action(a: ReferentialAction) -> Option<&'static str> {
    match a {
        ReferentialAction::NoAction => None,
        ReferentialAction::Cascade => Some("CASCADE"),
        ReferentialAction::SetNull => Some("SET NULL"),
        ReferentialAction::SetDefault => Some("SET DEFAULT"),
    }
}

/// 外部キー 1 件の `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY` 文を作る。
pub(super) fn render_foreign_key<E>(
    ctx: &Ctx<'_>,
    table_key: &str,
    fk: &ForeignKeyDef,
) -> Result<String, SchemaDdlError<E>> {
    if fk.references_parent_id() {
        return Err(unsupported(
            "foreign key",
            "reference to an implicit id row identity",
        ));
    }
    let mut s = String::from("ALTER TABLE ");
    s.push_str(&ctx.namer.qualified_required::<E>(table_key)?);
    s.push_str(" ADD CONSTRAINT ");
    push_ident(&mut s, fk.name())?;
    s.push_str(" FOREIGN KEY ");
    s.push_str(&render_column_list::<E>(fk.columns())?);
    s.push_str(" REFERENCES ");
    s.push_str(&ctx.namer.qualified_required::<E>(fk.parent_table())?);
    s.push(' ');
    s.push_str(&render_column_list::<E>(fk.parent_columns())?);
    match fk.match_type() {
        ForeignKeyMatch::Simple => {}
        ForeignKeyMatch::Full => s.push_str(" MATCH FULL"),
    }
    if let Some(a) = render_action(fk.on_delete()) {
        s.push_str(" ON DELETE ");
        s.push_str(a);
    }
    if let Some(a) = render_action(fk.on_update()) {
        s.push_str(" ON UPDATE ");
        s.push_str(a);
    }
    match fk.deferrability() {
        ForeignKeyDeferrability::NotDeferrable => {}
        ForeignKeyDeferrability::DeferrableInitiallyImmediate => {
            s.push_str(" DEFERRABLE INITIALLY IMMEDIATE");
        }
        ForeignKeyDeferrability::DeferrableInitiallyDeferred => {
            s.push_str(" DEFERRABLE INITIALLY DEFERRED");
        }
    }
    Ok(s)
}

/// 索引宣言 1 件の `CREATE [UNIQUE] INDEX` 文を作る。`table` は対象表のスキーマ（HNSW の演算子クラスの
/// 系列を列型から決めるために使う）。
pub(super) fn render_create_index<E>(
    ctx: &Ctx<'_>,
    def: &IndexDef,
    table: &TableSchema,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::from("CREATE ");
    if def.unique {
        s.push_str("UNIQUE ");
    }
    s.push_str("INDEX ");
    // 索引名は表と同じ SCHEMA に属する。名前部だけを識別子として出す。
    let name_part = super::super::namespace::relation_name_part(&def.name);
    push_ident(&mut s, name_part)?;
    s.push_str(" ON ");
    s.push_str(&ctx.namer.qualified_required::<E>(&def.table)?);
    match def.kind {
        IndexKind::Scalar => {}
        IndexKind::Hnsw(_) => s.push_str(" USING hnsw"),
        IndexKind::Gin(_) => s.push_str(" USING gin"),
    }
    s.push_str(" (");
    for (i, col) in def.columns.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        match def.expressions.get(i).and_then(|e| e.as_ref()) {
            Some(expr) => {
                s.push('(');
                s.push_str(&ctx.namer.externalize::<E>("index expression", &expr.sql)?);
                s.push(')');
            }
            None => {
                if col == "id" && !table.columns.iter().any(|c| c.name == "id") {
                    return Err(unsupported("index", "index on an implicit id column"));
                }
                push_ident(&mut s, col)?;
            }
        }
        match def.kind {
            IndexKind::Scalar => {}
            IndexKind::Hnsw(metric) => {
                let ty = table
                    .columns
                    .iter()
                    .find(|c| &c.name == col)
                    .map(|c| &c.ty)
                    .ok_or_else(|| unsupported("index", "index column is missing"))?;
                let family = HnswOpClassFamily::of_column_type(ty)
                    .ok_or_else(|| unsupported("index", "hnsw on a non-vector column"))?;
                s.push(' ');
                s.push_str(family.opclass_name(metric));
            }
            IndexKind::Gin(op) => {
                s.push(' ');
                s.push_str(op.as_str());
            }
        }
        if let Some(Some(c)) = def.collations.get(i) {
            s.push_str(" COLLATE ");
            push_collation(&mut s, *c);
        }
    }
    s.push(')');
    if let Some(opts) = def.hnsw_options {
        s.push_str(&format!(
            " WITH (m = {}, ef_construction = {})",
            opts.m(),
            opts.ef_construction()
        ));
    }
    if let Some(p) = &def.predicate {
        s.push_str(" WHERE ");
        s.push_str(&ctx.namer.externalize::<E>("index predicate", &p.sql)?);
    }
    Ok(s)
}

/// VIEW 1 件の `CREATE VIEW` 文を作る。
pub(super) fn render_create_view<E>(
    ctx: &Ctx<'_>,
    view_key: &str,
    def: &ViewDef,
) -> Result<String, SchemaDdlError<E>> {
    let body = ctx.namer.externalize::<E>("view", &def.body_sql)?;
    let mut s = String::from("CREATE VIEW ");
    s.push_str(&ctx.namer.qualified_required::<E>(view_key)?);
    if !def.column_aliases.is_empty() {
        s.push(' ');
        s.push_str(&render_column_list::<E>(&def.column_aliases)?);
    }
    s.push_str(" AS ");
    s.push_str(&body);
    match def.check_option {
        ViewCheckOption::None => {}
        ViewCheckOption::Local => s.push_str(" WITH LOCAL CHECK OPTION"),
        ViewCheckOption::Cascaded => s.push_str(" WITH CASCADED CHECK OPTION"),
    }
    Ok(s)
}
