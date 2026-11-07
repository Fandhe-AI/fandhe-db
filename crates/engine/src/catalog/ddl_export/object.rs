//! 書き出しの組み立て（スキーマ・ENUM・SEQUENCE・表・VIEW・索引の走査と順序づけ。RECOVER-16 (1)。
//! 親モジュール: [`super`]）。
//!
//! 責務境界: 読み取りスナップショットからカタログ表を走査し、依存順（スキーマ → ロール → ENUM →
//! SEQUENCE → 表 → VIEW → 関数 → 〔データ〕 → 外部キー → 索引 → SEQUENCE の現在値 → トリガ → 行セキュリティ・
//! ポリシー → 権限）に [`Emitter`] へ文を渡す。文字列の組み立ては [`super::table`]・[`super::access`]、
//! クォートは [`super::quote`] が担う。他テナントの SCHEMA 配下の relation は黙って対象外にする。

use std::collections::{BTreeMap, BTreeSet};

use redb::{ReadableTable, TableDefinition};

use super::super::sequence::{list_sequence_definitions_in, sequence_state_in};
use super::super::{
    decode_enum_type_def, decode_index_def, decode_view_def, get_table_schema_in_txn_impl,
    view_body_relations, CatalogError, EnumTypeDef, IndexDef, TableSchema, ViewDef, CATALOG_TABLE,
    ENUM_TYPES_TABLE, INDEX_CATALOG_TABLE, MAX_LIST_TABLES, VIEWS_TABLE,
};
use super::quote::{literal, push_ident, KeyScope};
use super::table::{
    render_create_index, render_create_table, render_create_view, render_foreign_key, SequenceMap,
};
use super::{access, unsupported, Ctx, DdlObjectKind, DdlSection, Emitter, SchemaDdlError};

/// `&str` キーの表を（上限付きで）すべて `(キー, 値)` で読む。表が無ければ空。
fn read_str_table<E>(
    ctx: &Ctx<'_>,
    def: TableDefinition<&str, &[u8]>,
) -> Result<Vec<(String, Vec<u8>)>, SchemaDdlError<E>> {
    let table = match ctx.rt.open_table(def) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    let mut out = Vec::new();
    for entry in table.iter().map_err(CatalogError::from)? {
        if out.len() >= MAX_LIST_TABLES {
            return Err(SchemaDdlError::Catalog(CatalogError::CorruptSchema(
                "catalog table exceeds the entry limit".to_string(),
            )));
        }
        let (k, v) = entry.map_err(CatalogError::from)?;
        out.push((k.value().to_string(), v.value().to_vec()));
    }
    Ok(out)
}

/// 書き出し対象に入る relation か（共有、または選択したテナントの SCHEMA 配下）。
/// 他テナントのキーは `false`（存在を示す出力・エラーを作らない）。
fn in_scope<E>(ctx: &Ctx<'_>, key: &str) -> Result<bool, SchemaDdlError<E>> {
    Ok(match ctx.namer.scope::<E>(key)? {
        KeyScope::Shared => ctx.scope.includes_shared(),
        KeyScope::Tenant { .. } => ctx.scope.tenant().is_some(),
        KeyScope::Foreign => false,
    })
}

/// 範囲内の表のキー一覧（昇順）。
fn table_keys<E>(ctx: &Ctx<'_>) -> Result<Vec<String>, SchemaDdlError<E>> {
    let mut keys = Vec::new();
    for (k, _) in read_str_table::<E>(ctx, CATALOG_TABLE)? {
        if in_scope(ctx, &k)? {
            keys.push((ctx.namer.sort_key::<E>(&k)?, k));
        }
    }
    keys.sort();
    Ok(keys.into_iter().map(|(_, k)| k).collect())
}

/// データ書き出し（[`super::data`]）の対象になる表のキー一覧（昇順）。行はすべてテナント所有のため、
/// 共有の表の行も範囲のテナント分は含める（範囲が共有の定義を出すかどうかとは独立）。他テナントの
/// SCHEMA 配下の表は存在を示さず黙って対象外にする。
pub(super) fn data_table_keys<E>(ctx: &Ctx<'_>) -> Result<Vec<String>, SchemaDdlError<E>> {
    let mut keys = Vec::new();
    for (k, _) in read_str_table::<E>(ctx, CATALOG_TABLE)? {
        match ctx.namer.scope::<E>(&k)? {
            KeyScope::Shared | KeyScope::Tenant { .. } => {
                keys.push((ctx.namer.sort_key::<E>(&k)?, k));
            }
            KeyScope::Foreign => {}
        }
    }
    keys.sort();
    Ok(keys.into_iter().map(|(_, k)| k).collect())
}

/// 範囲内の索引宣言（名前の昇順）。
fn index_defs<E>(ctx: &Ctx<'_>) -> Result<Vec<IndexDef>, SchemaDdlError<E>> {
    let mut out = Vec::new();
    for (k, v) in read_str_table::<E>(ctx, INDEX_CATALOG_TABLE)? {
        if in_scope(ctx, &k)? {
            out.push((ctx.namer.sort_key::<E>(&k)?, decode_index_def(&k, &v)?));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out.into_iter().map(|(_, d)| d).collect())
}

/// 全 SEQUENCE の定義と所有者（relation キーで引く。他テナントの分も含むが、出力は範囲で絞る）。
fn sequence_map<E>(ctx: &Ctx<'_>) -> Result<SequenceMap, SchemaDdlError<E>> {
    let mut m = SequenceMap::new();
    for (key, def, owner) in list_sequence_definitions_in(ctx.rt)? {
        m.insert(key, (def, owner));
    }
    Ok(m)
}

/// `PreData` の文を渡す。
pub(super) fn export_pre_data<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    export_schemas(ctx, out)?;
    access::export_roles(ctx, out)?;
    export_enums(ctx, out)?;
    let seqs = sequence_map::<E>(ctx)?;
    let mut consumed: BTreeSet<String> = BTreeSet::new();
    export_standalone_sequences(ctx, out, &seqs)?;
    export_tables(ctx, out, &seqs, &mut consumed)?;
    ensure_owned_sequences_consumed(ctx, &seqs, &consumed)?;
    reject_materialized_views(ctx)?;
    export_views(ctx, out)?;
    access::export_functions(ctx, out)?;
    Ok(())
}

/// `PostData` の文を渡す。
pub(super) fn export_post_data<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    export_foreign_keys(ctx, out)?;
    export_indexes(ctx, out)?;
    let seqs = sequence_map::<E>(ctx)?;
    export_sequence_values(ctx, out, &seqs)?;
    access::export_triggers(ctx, out)?;
    access::export_row_security_and_policies(ctx, out)?;
    Ok(())
}

/// 選択したテナントの `CREATE SCHEMA`。
fn export_schemas<E>(ctx: &Ctx<'_>, out: &mut Emitter<'_, E>) -> Result<(), SchemaDdlError<E>> {
    // 一覧の順は SCHEMA 名の昇順（内部 ID の順に依存しない。再生成した文が空 DB への復元後も一致する）。
    let mut names: Vec<&String> = ctx.namer.schemas.values().collect();
    names.sort();
    for name in names {
        let mut s = String::from("CREATE SCHEMA ");
        push_ident(&mut s, name)?;
        out.emit(DdlSection::PreData, DdlObjectKind::Schema, &s)?;
    }
    Ok(())
}

/// 共有の ENUM 型。
fn export_enums<E>(ctx: &Ctx<'_>, out: &mut Emitter<'_, E>) -> Result<(), SchemaDdlError<E>> {
    for (k, v) in read_str_table::<E>(ctx, ENUM_TYPES_TABLE)? {
        // キーの所属で絞る（デコードより前。他テナントの SCHEMA 配下の型は存在を示さず無視する）。
        match ctx.namer.scope::<E>(&k)? {
            KeyScope::Shared if ctx.scope.includes_shared() => {}
            KeyScope::Shared | KeyScope::Foreign => continue,
            // `CREATE TYPE` は SCHEMA 修飾名を受理しないため、SCHEMA 配下の型は戻せない。
            KeyScope::Tenant { .. } => {
                return Err(unsupported(
                    "enum type",
                    "schema-scoped enum type cannot be recreated by SQL",
                ));
            }
        }
        let def: EnumTypeDef = decode_enum_type_def(&k, &v)?;
        let mut s = String::from("CREATE TYPE ");
        push_ident(&mut s, def.name())?;
        s.push_str(" AS ENUM (");
        for (i, label) in def.labels().iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&literal::<E>(label)?);
        }
        s.push(')');
        out.emit(DdlSection::PreData, DdlObjectKind::EnumType, &s)?;
    }
    Ok(())
}

/// 所有者のいない SEQUENCE（`CREATE SEQUENCE`）。所有される SEQUENCE（SERIAL・IDENTITY）は表の列定義が
/// 作り直すので出さない。
fn export_standalone_sequences<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
    seqs: &SequenceMap,
) -> Result<(), SchemaDdlError<E>> {
    for (key, (def, owner)) in seqs {
        if owner.is_some() || !in_scope(ctx, key)? {
            continue;
        }
        if !matches!(ctx.namer.scope::<E>(key)?, KeyScope::Shared) {
            // `CREATE SEQUENCE` は SCHEMA 修飾名を受理しないため、SCHEMA 配下の単独 SEQUENCE は戻せない。
            return Err(unsupported(
                "sequence",
                "schema-scoped standalone sequence cannot be recreated by SQL",
            ));
        }
        let p = &def.params;
        let mut s = String::from("CREATE SEQUENCE ");
        push_ident(&mut s, key)?;
        s.push_str(&format!(
            " START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {}",
            p.start,
            p.increment,
            p.min_value,
            p.max_value,
            if p.cycle { "CYCLE" } else { "NO CYCLE" }
        ));
        out.emit(DdlSection::PreData, DdlObjectKind::Sequence, &s)?;
        if def.restart_value != p.start {
            let mut r = String::from("ALTER SEQUENCE ");
            push_ident(&mut r, key)?;
            r.push_str(&format!(" RESTART WITH {}", def.restart_value));
            out.emit(DdlSection::PreData, DdlObjectKind::Sequence, &r)?;
        }
    }
    Ok(())
}

/// 範囲内の表（`CREATE TABLE`）。
fn export_tables<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
    seqs: &SequenceMap,
    consumed: &mut BTreeSet<String>,
) -> Result<(), SchemaDdlError<E>> {
    let unique_indexes = index_defs::<E>(ctx)?;
    for key in table_keys::<E>(ctx)? {
        let schema = get_table_schema_in_txn_impl(ctx.rt, &key, true)?;
        for col in &schema.columns {
            if let Some(name) = col.default_expr.as_ref().and_then(|d| d.sequence_name()) {
                consumed.insert(name.to_string());
            }
        }
        let uniq_names: Vec<String> = unique_indexes
            .iter()
            .filter(|d| d.unique && d.table == key)
            .map(|d| super::super::namespace::relation_name_part(&d.name).to_string())
            .collect();
        let sql = render_create_table(ctx, &key, &schema, seqs, &uniq_names)?;
        out.emit(DdlSection::PreData, DdlObjectKind::Table, &sql)?;
    }
    Ok(())
}

/// 範囲内の所有される SEQUENCE が、すべていずれかの列の SERIAL／IDENTITY として描画されたことを確かめる
/// （列に結びつかない所有 SEQUENCE は SQL で戻せない）。
fn ensure_owned_sequences_consumed<E>(
    ctx: &Ctx<'_>,
    seqs: &SequenceMap,
    consumed: &BTreeSet<String>,
) -> Result<(), SchemaDdlError<E>> {
    for (key, (_, owner)) in seqs {
        if owner.is_some() && in_scope(ctx, key)? && !consumed.contains(key) {
            return Err(unsupported(
                "sequence",
                "owned sequence is not attached to a column default",
            ));
        }
    }
    Ok(())
}

/// マテリアライズド VIEW は定義と実体（文脈ごとの行）が別で、実体を DDL へ戻す手段が無いため、
/// 1 件でもあれば書き出せない（黙って省かない）。
fn reject_materialized_views<E>(ctx: &Ctx<'_>) -> Result<(), SchemaDdlError<E>> {
    // 他の種別と同じく relation キーの所属で絞る。範囲内（共有分を選んだときの共有、または選択した
    // テナントの SCHEMA 配下）に 1 件でもあれば失敗し、範囲外（他テナント）は存在を示さず無視する。
    for (k, _) in read_str_table::<E>(ctx, super::super::materialized::MATERIALIZED_VIEWS_TABLE)? {
        if in_scope(ctx, &k)? {
            return Err(unsupported(
                "materialized view",
                "materialized views cannot be recreated by the schema dump",
            ));
        }
    }
    Ok(())
}

/// 範囲内の VIEW。ビューが参照するビューを先に出す（トポロジカル順）。
fn export_views<E>(ctx: &Ctx<'_>, out: &mut Emitter<'_, E>) -> Result<(), SchemaDdlError<E>> {
    let mut pending: BTreeMap<String, ViewDef> = BTreeMap::new();
    for (k, v) in read_str_table::<E>(ctx, VIEWS_TABLE)? {
        if in_scope(ctx, &k)? {
            pending.insert(k, decode_view_def(&v)?);
        }
    }
    // 各 VIEW が読む relation をすべて集める。保存済み本文の構文検証（DROP の依存列挙と同じ
    // `view_body_relations`）で UNION／INTERSECT／EXCEPT の各枝・JOIN・部分問い合わせ・CTE の参照まで得る。
    // 導出できない本文は、並べ方を推測せず全体を失敗させる。
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, d) in &pending {
        let mut rels = view_body_relations(&d.body_sql)
            .map_err(|_| unsupported("view", "view body references cannot be determined"))?;
        rels.push(d.base_relation.clone());
        deps.insert(k.clone(), rels);
    }
    while !pending.is_empty() {
        let mut ready: Vec<((String, String), String)> = Vec::new();
        for k in pending.keys() {
            if !deps
                .get(k)
                .is_some_and(|ds| ds.iter().any(|r| pending.contains_key(r)))
            {
                ready.push((ctx.namer.sort_key::<E>(k)?, k.clone()));
            }
        }
        if ready.is_empty() {
            return Err(unsupported("view", "views reference each other in a cycle"));
        }
        ready.sort();
        for (_, key) in ready {
            if let Some(def) = pending.remove(&key) {
                let sql = render_create_view(ctx, &key, &def)?;
                out.emit(DdlSection::PreData, DdlObjectKind::View, &sql)?;
            }
        }
    }
    Ok(())
}

/// 範囲内の外部キー。
fn export_foreign_keys<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    for key in table_keys::<E>(ctx)? {
        let schema: TableSchema = get_table_schema_in_txn_impl(ctx.rt, &key, true)?;
        for fk in schema.foreign_keys() {
            let sql = render_foreign_key(ctx, &key, fk)?;
            out.emit(DdlSection::PostData, DdlObjectKind::ForeignKey, &sql)?;
        }
    }
    Ok(())
}

/// 範囲内の索引宣言。
fn export_indexes<E>(ctx: &Ctx<'_>, out: &mut Emitter<'_, E>) -> Result<(), SchemaDdlError<E>> {
    for def in index_defs::<E>(ctx)? {
        // 対象が表でない索引（マテリアライズド VIEW 上など）は戻せない。
        let table = match get_table_schema_in_txn_impl(ctx.rt, &def.table, true) {
            Ok(t) => t,
            Err(CatalogError::TableNotFound(_)) => {
                return Err(unsupported(
                    "index",
                    "index on a relation that is not a table",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        let sql = render_create_index(ctx, &def, &table)?;
        out.emit(DdlSection::PostData, DdlObjectKind::Index, &sql)?;
    }
    Ok(())
}

/// 選択したテナントの SEQUENCE 現在値（`setval`）。状態行が無い（まだ採番していない）SEQUENCE は出さない。
fn export_sequence_values<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
    seqs: &SequenceMap,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    for (key, (def, _)) in seqs {
        // 他テナントの SCHEMA 配下の SEQUENCE は存在を示さず飛ばす。
        if matches!(ctx.namer.scope::<E>(key)?, KeyScope::Foreign) {
            continue;
        }
        // 共有の SEQUENCE は共有分を選んでいなくても、選択したテナントの現在値は出す。
        let Some((last, called)) = sequence_state_in(ctx.rt, tenant, def.id)? else {
            continue;
        };
        // `setval` は SCHEMA 修飾名・引用符付きの名前を受理しないため、現在値を持つ SCHEMA 配下の
        // SEQUENCE の現在値は戻せない（現在値を持たなければ出す文が無く、省略にならない）。
        let arg = ctx
            .namer
            .sequence_arg::<E>(key)?
            .ok_or_else(|| unsupported("sequence", "sequence is outside the dump scope"))?;
        let sql = format!(
            "SELECT setval({}, {last}, {})",
            literal::<E>(&arg)?,
            if called { "TRUE" } else { "FALSE" }
        );
        out.emit(DdlSection::PostData, DdlObjectKind::SequenceValue, &sql)?;
    }
    Ok(())
}
