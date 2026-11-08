//! 書き出しの組み立て（スキーマ・ENUM・SEQUENCE・表・VIEW・索引の走査と順序づけ。RECOVER-16 (1)。
//! 親モジュール: [`super`]）。
//!
//! 責務境界: 読み取りスナップショットからカタログ表を走査し、依存順（スキーマ → ロール → ENUM →
//! SEQUENCE → 表 → VIEW → 関数 → 〔データ〕 → 外部キー → 索引 → SEQUENCE の現在値 → トリガ → 行セキュリティ・
//! ポリシー → 権限）に [`Emitter`] へ文を渡す。文字列の組み立ては [`super::table`]・[`super::access`]、
//! クォートは [`super::quote`] が担う。他テナントの SCHEMA 配下の relation は黙って対象外にする。

use std::collections::{BTreeMap, BTreeSet};

use redb::{ReadableTable, TableDefinition};

use super::super::materialized::{
    decode_materialized_view_def, tenant_has_entity_in, MaterializedViewDef,
    MATERIALIZED_VIEWS_TABLE,
};
use super::super::sequence::{list_sequence_definitions_in, sequence_state_in};
use super::super::{
    decode_enum_type_def, decode_index_def, decode_view_def, get_table_schema_in_txn_impl,
    view_body_relations, CatalogError, EnumTypeDef, IndexDef, TableSchema, ViewDef, CATALOG_TABLE,
    ENUM_TYPES_TABLE, INDEX_CATALOG_TABLE, MAX_LIST_TABLES, VIEWS_TABLE,
};
use super::quote::{literal, push_ident, KeyScope};
use super::table::{
    render_create_index, render_create_materialized_view, render_create_table, render_create_view,
    render_foreign_key, SequenceMap,
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
    // マテリアライズド VIEW の `WITH NO DATA` は本文を解析して列型を保存するため、問合せが呼ぶ
    // テナント関数が先に在る必要がある。マテリアライズド VIEW とそれを読む VIEW は関数の後ろへ回す。
    export_views(ctx, out, false)?;
    access::export_functions(ctx, out)?;
    export_views(ctx, out, true)?;
    Ok(())
}

/// 論理ダンプの末尾（`PostData` の後）に出す、マテリアライズド VIEW の再計算（`REFRESH`）。
/// スキーマだけの書き出し（`Storage::export_schema_ddl`）では呼ばない（表が空のまま
/// 「実体化済み」にすると、元の未実体化の状態より緩くなるため）。Issue #3097。
pub(super) fn export_matview_refreshes<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    // 共有の `populated` は他テナントの REFRESH でも変わるため使わず、選択したテナント自身の
    // 実体があるものだけ出す（`export_sequence_values` が自テナントの状態行だけを見るのと同じ扱い）。
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    let (ordered, deps) = view_like_order_with_deps::<E>(ctx)?;
    // 依存先まで含めた再計算可否の検査用。トポロジカル順に、実体を持たない matview か、
    // そうしたものを（VIEW 経由を含め）読む定義を「未実体化に依存」として印を付ける。
    // 実体を持つ matview がこの印を持つと、復元先の `REFRESH` が未実体化の relation を読んで
    // 失敗する（55000）ため、復元不能なダンプを正常出力せず全体を失敗させる。
    let mut tainted: BTreeSet<String> = BTreeSet::new();
    for (key, kind) in &ordered {
        let reads_tainted = deps
            .get(key)
            .is_some_and(|ds| ds.iter().any(|r| tainted.contains(r)));
        if matches!(kind, ViewLike::Materialized(_)) {
            if tenant_has_entity_in(ctx.rt, key, tenant)? {
                if reads_tainted {
                    return Err(unsupported(
                        "materialized view",
                        "populated materialized view depends on an unpopulated materialized view",
                    ));
                }
                let sql = format!(
                    "REFRESH MATERIALIZED VIEW {}",
                    ctx.namer.qualified_required::<E>(key)?
                );
                out.emit(
                    DdlSection::PostData,
                    DdlObjectKind::MaterializedViewRefresh,
                    &sql,
                )?;
            } else {
                tainted.insert(key.clone());
            }
        } else if reads_tainted {
            tainted.insert(key.clone());
        }
    }
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
    // 出力順は SCHEMA の内部 ID ではなく描画名（SCHEMA 名, 名前）で決める（往復で同じ DDL 列にする）。
    let mut targets = Vec::new();
    for (key, (def, owner)) in seqs {
        if owner.is_some() || !in_scope(ctx, key)? {
            continue;
        }
        targets.push((ctx.namer.sort_key::<E>(key)?, key, def));
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, key, def) in targets {
        // 共有は `"name"`、SCHEMA 配下は引用符なしの `schema.name`（SCHEMA 修飾名は引用符付きを受理しない）。
        let name = ctx.namer.qualified_required::<E>(key)?;
        let p = &def.params;
        let s = format!(
            "CREATE SEQUENCE {name} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {}",
            p.start,
            p.increment,
            p.min_value,
            p.max_value,
            if p.cycle { "CYCLE" } else { "NO CYCLE" }
        );
        out.emit(DdlSection::PreData, DdlObjectKind::Sequence, &s)?;
        if def.restart_value != p.start {
            let r = format!("ALTER SEQUENCE {name} RESTART WITH {}", def.restart_value);
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

/// VIEW とマテリアライズド VIEW のどちらか（依存順の 1 要素）。
enum ViewLike {
    View(ViewDef),
    Materialized(MaterializedViewDef),
}

/// 範囲内の VIEW・マテリアライズド VIEW を、読む relation を先に出すトポロジカル順で返す
/// （VIEW が matview を、matview が VIEW・matview を読めるため 1 つの依存グラフで並べる）。
/// 準備できたものは描画名の順に出す。導出できない本文・循環は全体を失敗させる。
/// 並び順に加えて、各定義が読む relation の一覧（依存辺）も返す。
/// 出力順の分割（`export_views`）と matview の再計算可否の検査（`export_matview_refreshes`）が使う。
#[allow(clippy::type_complexity)]
fn view_like_order_with_deps<E>(
    ctx: &Ctx<'_>,
) -> Result<(Vec<(String, ViewLike)>, BTreeMap<String, Vec<String>>), SchemaDdlError<E>> {
    let mut pending: BTreeMap<String, ViewLike> = BTreeMap::new();
    for (k, v) in read_str_table::<E>(ctx, VIEWS_TABLE)? {
        if in_scope(ctx, &k)? {
            pending.insert(k, ViewLike::View(decode_view_def(&v)?));
        }
    }
    for (k, v) in read_str_table::<E>(ctx, MATERIALIZED_VIEWS_TABLE)? {
        if in_scope(ctx, &k)? {
            // `CREATE MATERIALIZED VIEW` は SCHEMA 修飾名を受理しない（共有にしか作れない）ため、
            // SCHEMA 配下の定義は戻せない。デコードより前に、固定文言で全体を失敗させる。
            if matches!(ctx.namer.scope::<E>(&k)?, KeyScope::Tenant { .. }) {
                return Err(unsupported(
                    "materialized view",
                    "schema-scoped materialized view cannot be recreated by SQL",
                ));
            }
            let def = decode_materialized_view_def(&k, &v)?;
            pending.insert(k, ViewLike::Materialized(def));
        }
    }
    // 各定義が読む relation をすべて集める。保存済み本文の構文検証（DROP の依存列挙と同じ
    // `view_body_relations`）で UNION／INTERSECT／EXCEPT の各枝・JOIN・部分問い合わせ・CTE の参照まで得る。
    // 導出できない本文は、並べ方を推測せず全体を失敗させる。
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, d) in &pending {
        let (body, mut rels) = match d {
            ViewLike::View(v) => (&v.body_sql, vec![v.base_relation.clone()]),
            ViewLike::Materialized(m) => (&m.body_sql, m.base_relations.clone()),
        };
        rels.extend(
            view_body_relations(body)
                .map_err(|_| unsupported("view", "view body references cannot be determined"))?,
        );
        deps.insert(k.clone(), rels);
    }
    let mut ordered = Vec::with_capacity(pending.len());
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
                ordered.push((key, def));
            }
        }
    }
    Ok((ordered, deps))
}

/// 範囲内のマテリアライズド VIEW の定義を返す（論理ダンプ前の列構成の照合用。Issue #3097）。
/// SCHEMA 配下の定義は戻せないため、`view_like_order_with_deps` と同じく固定文言で全体を失敗させる。
pub(super) fn in_scope_materialized_defs<E>(
    ctx: &Ctx<'_>,
) -> Result<Vec<MaterializedViewDef>, SchemaDdlError<E>> {
    let mut out = Vec::new();
    for (k, v) in read_str_table::<E>(ctx, MATERIALIZED_VIEWS_TABLE)? {
        if in_scope(ctx, &k)? {
            if matches!(ctx.namer.scope::<E>(&k)?, KeyScope::Tenant { .. }) {
                return Err(unsupported(
                    "materialized view",
                    "schema-scoped materialized view cannot be recreated by SQL",
                ));
            }
            out.push(decode_materialized_view_def(&k, &v)?);
        }
    }
    Ok(out)
}

/// 範囲内の VIEW とマテリアライズド VIEW。読む relation を先に出す（トポロジカル順）。
/// マテリアライズド VIEW は `WITH NO DATA`（実体は運ばない）で出す。
/// `after_functions` が偽なら関数より前に出す分（マテリアライズド VIEW に依存しない VIEW）、
/// 真なら関数の後ろに出す分（マテリアライズド VIEW と、それを（推移的に）読む VIEW）。
fn export_views<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
    after_functions: bool,
) -> Result<(), SchemaDdlError<E>> {
    let (ordered, deps) = view_like_order_with_deps::<E>(ctx)?;
    // トポロジカル順に、マテリアライズド VIEW かそれを読む定義へ印を付ける。
    let mut deferred: BTreeSet<String> = BTreeSet::new();
    for (key, def) in &ordered {
        let reads_deferred = deps
            .get(key)
            .is_some_and(|ds| ds.iter().any(|r| deferred.contains(r)));
        if reads_deferred || matches!(def, ViewLike::Materialized(_)) {
            deferred.insert(key.clone());
        }
    }
    for (key, def) in ordered {
        if deferred.contains(&key) != after_functions {
            continue;
        }
        match def {
            ViewLike::View(v) => {
                let sql = render_create_view(ctx, &key, &v)?;
                out.emit(DdlSection::PreData, DdlObjectKind::View, &sql)?;
            }
            ViewLike::Materialized(m) => {
                let sql = render_create_materialized_view(ctx, &key, &m)?;
                out.emit(DdlSection::PreData, DdlObjectKind::MaterializedView, &sql)?;
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
    let mut matviews: Option<BTreeMap<String, MaterializedViewDef>> = None;
    for def in index_defs::<E>(ctx)? {
        // 対象が表でなければマテリアライズド VIEW の定義を引く（どちらでもなければ戻せない）。
        let table = match get_table_schema_in_txn_impl(ctx.rt, &def.table, true) {
            Ok(t) => t,
            Err(CatalogError::TableNotFound(_)) => {
                if matviews.is_none() {
                    let mut m = BTreeMap::new();
                    for (k, v) in read_str_table::<E>(ctx, MATERIALIZED_VIEWS_TABLE)? {
                        let d = decode_materialized_view_def(&k, &v)?;
                        m.insert(k, d);
                    }
                    matviews = Some(m);
                }
                let Some(mv) = matviews.as_ref().and_then(|m| m.get(&def.table)) else {
                    return Err(unsupported(
                        "index",
                        "index on a relation that is neither a table nor a materialized view",
                    ));
                };
                TableSchema::new(def.table.clone(), mv.columns.clone())
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
    let mut targets = Vec::new();
    for (key, (def, _)) in seqs {
        // 他テナントの SCHEMA 配下の SEQUENCE は存在を示さず飛ばす。
        if matches!(ctx.namer.scope::<E>(key)?, KeyScope::Foreign) {
            continue;
        }
        targets.push((ctx.namer.sort_key::<E>(key)?, key, def));
    }
    // 出力順は描画名で決める（SCHEMA の内部 ID に依存させない）。
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, key, def) in targets {
        // 共有の SEQUENCE は共有分を選んでいなくても、選択したテナントの現在値は出す。状態行は
        // テナント指定で引くため、他テナントの現在値は読まない。
        let Some((last, called)) = sequence_state_in(ctx.rt, tenant, def.id)? else {
            continue;
        };
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
