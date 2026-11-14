//! ロール・関数・トリガ・行セキュリティ・ポリシーの DDL 生成（RECOVER-16 (1)。親モジュール: [`super`]）。
//!
//! 責務境界: テナント固有のオブジェクト（キーの先頭がテナント）を、選択したテナントの分だけ書き出す。
//! メンバーシップ・権限（ACL）・所有者・既定権限・初期権限・旧形式ハッシュは SQL の文では戻せないため、
//! 復元専用の指示文として [`super::privilege`] が書き出す（Issue #3096）。ロールの `CREATE ROLE` は
//! SCRAM 検証子を含めてここで出す。

use redb::ReadableTable;

use super::super::function::{
    list_functions_in_source, FunctionDef, FunctionLanguage, Parallel, ReturnShape, Security,
    Volatility,
};
use super::super::policy::{list_policies_in_source, PolicyCommand, PolicyDef, PolicyRole};
use super::super::role::list_roles_in_txn;
use super::super::row_security::row_security_state_in_txn;
use super::super::trigger::{
    list_triggers_in_source, TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming,
};
use super::super::{CatalogError, ColumnType, CATALOG_TABLE};
use super::quote::{ident, literal, push_ident, KeyScope};
use super::table::render_column_type;
use super::{unsupported, Ctx, DdlObjectKind, DdlSection, Emitter, SchemaDdlError};

/// 選択したテナントの表（共有の表 + そのテナントの SCHEMA 配下の表）のキー一覧。テナント固有の
/// トリガ・ポリシー・行セキュリティは、共有の表に対しても各テナントが別々に持つ。
fn tenant_table_keys<E>(ctx: &Ctx<'_>) -> Result<Vec<String>, SchemaDdlError<E>> {
    let table = match ctx.rt.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    let mut keys = Vec::new();
    for entry in table.iter().map_err(CatalogError::from)? {
        let (k, _) = entry.map_err(CatalogError::from)?;
        match ctx.namer.scope::<E>(k.value())? {
            KeyScope::Shared | KeyScope::Tenant { .. } => {
                keys.push((ctx.namer.sort_key::<E>(k.value())?, k.value().to_string()))
            }
            KeyScope::Foreign => {}
        }
    }
    // 内部キーの順（SCHEMA の内部 ID 順）は復元先で変わるため、描画名で並べる。
    keys.sort();
    Ok(keys.into_iter().map(|(_, k)| k).collect())
}

/// 選択したテナントの `CREATE ROLE`（`PreData`）。旧形式ハッシュ・初期権限は [`super::privilege`] の指示文で戻す。
pub(super) fn export_roles<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    for role in list_roles_in_txn(ctx.rt, tenant)? {
        let mut s = String::from("CREATE ROLE ");
        push_ident(&mut s, &role.name)?;
        let a = role.attributes;
        s.push_str(if a.login { " LOGIN" } else { " NOLOGIN" });
        s.push_str(if a.inherit { " INHERIT" } else { " NOINHERIT" });
        s.push_str(if a.createrole {
            " CREATEROLE"
        } else {
            " NOCREATEROLE"
        });
        s.push_str(if a.createdb {
            " CREATEDB"
        } else {
            " NOCREATEDB"
        });
        if role.connection_limit >= 0 {
            s.push_str(&format!(" CONNECTION LIMIT {}", role.connection_limit));
        }
        if let Some(micros) = role.valid_until {
            let text = crate::datetime::format_timestamptz(
                micros,
                &crate::datetime::SessionTimeZone::utc(),
            );
            s.push_str(" VALID UNTIL ");
            s.push_str(&literal::<E>(&text)?);
        }
        if !role.scram_verifier.is_empty() {
            let v = std::str::from_utf8(&role.scram_verifier)
                .map_err(|_| unsupported("role", "password verifier is not text"))?;
            s.push_str(" PASSWORD ");
            s.push_str(&literal::<E>(v)?);
        }
        out.emit(DdlSection::PreData, DdlObjectKind::Role, &s)?;
    }
    Ok(())
}

/// 関数の型を `CREATE FUNCTION` の型名へ描画する。配列の要素数上限は関数の同一性に含まれないため
/// `[N]` を付けず `[]` で出す。
fn render_function_type<E>(ty: &ColumnType) -> Result<String, SchemaDdlError<E>> {
    match ty {
        ColumnType::Array(a) => {
            let rendered = render_column_type::<E>(ty)?;
            // `<要素>[<N>][]...` の `[<N>]` だけを `[]` に置き換える。
            let marker = format!("[{}]", a.max_len());
            Ok(rendered.replacen(&marker, "[]", 1))
        }
        other => render_column_type::<E>(other),
    }
}

/// 関数の署名 `<修飾名>(<型>, ...)`。ACL・所有者の指示文が関数を指す安定した名前で、書き出し側と
/// 復元側（[`super::restore_apply`]）が同じ関数で描画して一致を取る（内部 ID は復元先で変わるため）。
pub(super) fn function_signature<E>(
    namer: &super::quote::Namer,
    def: &FunctionDef,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = namer.qualified_required::<E>(&def.function_key)?;
    s.push('(');
    for (i, p) in def.params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let ty =
            p.ty.as_ref()
                .ok_or_else(|| unsupported("function", "untyped parameter"))?;
        s.push_str(&render_fn_type::<E>(ty)?);
    }
    s.push(')');
    Ok(s)
}

fn render_fn_type<E>(
    t: &super::super::function::FunctionType,
) -> Result<String, SchemaDdlError<E>> {
    let ty = t.to_column_type().map_err(SchemaDdlError::Catalog)?;
    render_function_type::<E>(&ty)
}

/// 本体をドル引用符で囲む。タグは `$fandhe<n>$` で、開きタグの直後から最初に現れる同じタグが
/// 末尾の閉じタグそのものである値を選ぶ（本体の末尾と閉じタグが結合して早く閉じる形を避ける）。
pub(super) fn dollar_quote<E>(body: &str) -> Result<String, SchemaDdlError<E>> {
    for n in 0..=32u32 {
        let tag = if n == 0 {
            "$fandhe$".to_string()
        } else {
            format!("$fandhe{n}$")
        };
        let quoted = format!("{tag}{body}{tag}");
        let expected_close = quoted.len() - tag.len();
        if quoted.get(tag.len()..).and_then(|rest| rest.find(&tag))
            == Some(expected_close - tag.len())
        {
            return Ok(quoted);
        }
    }
    Err(unsupported(
        "function",
        "function body cannot be safely dollar-quoted",
    ))
}

/// 関数 1 件の `CREATE FUNCTION` 文を作る。
fn render_create_function<E>(
    ctx: &Ctx<'_>,
    def: &FunctionDef,
) -> Result<String, SchemaDdlError<E>> {
    match def.language {
        FunctionLanguage::Sql | FunctionLanguage::Plpgsql => {}
        FunctionLanguage::Declarative => {
            return Err(unsupported(
                "function",
                "declarative function cannot be recreated by CREATE FUNCTION",
            ));
        }
    }
    if def.standard_body {
        return Err(unsupported(
            "function",
            "function with a standard-form body cannot be recreated",
        ));
    }
    if matches!(def.attrs.security, Security::Definer) {
        return Err(unsupported("function", "SECURITY DEFINER is not supported"));
    }
    let mut s = String::from("CREATE FUNCTION ");
    s.push_str(&ctx.namer.qualified_required::<E>(&def.function_key)?);
    s.push('(');
    for (i, p) in def.params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let name = p
            .name
            .as_deref()
            .ok_or_else(|| unsupported("function", "unnamed parameter"))?;
        let ty =
            p.ty.as_ref()
                .ok_or_else(|| unsupported("function", "untyped parameter"))?;
        s.push_str(&ident::<E>(name)?);
        s.push(' ');
        s.push_str(&render_fn_type::<E>(ty)?);
    }
    s.push_str(") RETURNS ");
    match (&def.return_shape, &def.return_type) {
        (ReturnShape::Scalar, Some(t)) if t.is_trigger() => s.push_str("trigger"),
        (ReturnShape::Scalar, Some(t)) => s.push_str(&render_fn_type::<E>(t)?),
        (ReturnShape::SetOf, Some(t)) => {
            s.push_str("SETOF ");
            s.push_str(&render_fn_type::<E>(t)?);
        }
        (ReturnShape::Table(cols), _) => {
            s.push_str("TABLE(");
            for (i, (name, t)) in cols.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&ident::<E>(name)?);
                s.push(' ');
                s.push_str(&render_fn_type::<E>(t)?);
            }
            s.push(')');
        }
        (ReturnShape::Scalar | ReturnShape::SetOf, None) => {
            return Err(unsupported("function", "function without a result type"));
        }
    }
    s.push_str(match def.language {
        FunctionLanguage::Sql => " LANGUAGE sql",
        FunctionLanguage::Plpgsql => " LANGUAGE plpgsql",
        FunctionLanguage::Declarative => "",
    });
    s.push_str(match def.attrs.volatility {
        Volatility::Immutable => " IMMUTABLE",
        Volatility::Stable => " STABLE",
        Volatility::Volatile => " VOLATILE",
    });
    if def.attrs.strict {
        s.push_str(" STRICT");
    }
    s.push_str(match def.attrs.parallel {
        Parallel::Unsafe => " PARALLEL UNSAFE",
        Parallel::Restricted => " PARALLEL RESTRICTED",
        Parallel::Safe => " PARALLEL SAFE",
    });
    if let Some(c) = def.attrs.cost {
        s.push_str(&format!(" COST {c}"));
    }
    if let Some(r) = def.attrs.rows {
        s.push_str(&format!(" ROWS {r}"));
    }
    s.push_str(" AS ");
    s.push_str(&dollar_quote::<E>(&def.body_source)?);
    Ok(s)
}

/// 選択したテナントの `CREATE FUNCTION`。
pub(super) fn export_functions<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    let mut defs = Vec::new();
    for def in list_functions_in_source(ctx.rt, tenant)? {
        defs.push((ctx.namer.sort_key::<E>(&def.function_key)?, def));
    }
    // 同名の多重定義は元の順（シグネチャ順）を保つ（安定ソート）。
    defs.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, def) in defs {
        let sql = render_create_function(ctx, &def)?;
        out.emit(DdlSection::PreData, DdlObjectKind::Function, &sql)?;
    }
    Ok(())
}

fn render_trigger<E>(
    ctx: &Ctx<'_>,
    table_key: &str,
    def: &TriggerDef,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::from("CREATE TRIGGER ");
    push_ident(&mut s, &def.name)?;
    s.push_str(match def.timing {
        TriggerTiming::Before => " BEFORE ",
        TriggerTiming::After => " AFTER ",
    });
    let mut events: Vec<String> = Vec::new();
    if def.events.contains(TriggerEvents::INSERT) {
        events.push("INSERT".to_string());
    }
    if def.events.contains(TriggerEvents::UPDATE) {
        if def.update_of.is_empty() {
            events.push("UPDATE".to_string());
        } else {
            let mut cols = Vec::new();
            for c in &def.update_of {
                cols.push(ident::<E>(c)?);
            }
            events.push(format!("UPDATE OF {}", cols.join(", ")));
        }
    }
    if def.events.contains(TriggerEvents::DELETE) {
        events.push("DELETE".to_string());
    }
    if def.events.contains(TriggerEvents::TRUNCATE) {
        events.push("TRUNCATE".to_string());
    }
    s.push_str(&events.join(" OR "));
    s.push_str(" ON ");
    s.push_str(&ctx.namer.qualified_required::<E>(table_key)?);
    s.push_str(match def.level {
        TriggerLevel::Row => " FOR EACH ROW",
        TriggerLevel::Statement => " FOR EACH STATEMENT",
    });
    // WHEN 条件は NEW／OLD の参照だけを持つ正規化テキスト（表名・内部キーを含まない。Issue #3190）。
    if let Some(when) = &def.when_sql {
        s.push_str(" WHEN (");
        s.push_str(when);
        s.push(')');
    }
    s.push_str(" EXECUTE FUNCTION ");
    s.push_str(&ctx.namer.qualified_required::<E>(&def.function_key)?);
    s.push('(');
    for (i, a) in def.tg_args.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&literal::<E>(a)?);
    }
    s.push(')');
    Ok(s)
}

/// 選択したテナントの `CREATE TRIGGER`。
pub(super) fn export_triggers<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    for key in tenant_table_keys::<E>(ctx)? {
        for def in list_triggers_in_source(ctx.rt, tenant, &key)? {
            let sql = render_trigger(ctx, &key, &def)?;
            out.emit(DdlSection::PostData, DdlObjectKind::Trigger, &sql)?;
        }
    }
    Ok(())
}

fn render_policy<E>(
    ctx: &Ctx<'_>,
    table_key: &str,
    def: &PolicyDef,
) -> Result<String, SchemaDdlError<E>> {
    let mut s = String::from("CREATE POLICY ");
    push_ident(&mut s, &def.name)?;
    s.push_str(" ON ");
    s.push_str(&ctx.namer.qualified_required::<E>(table_key)?);
    s.push_str(if def.permissive {
        " AS PERMISSIVE"
    } else {
        " AS RESTRICTIVE"
    });
    s.push_str(match def.command {
        PolicyCommand::All => " FOR ALL",
        PolicyCommand::Select => " FOR SELECT",
        PolicyCommand::Insert => " FOR INSERT",
        PolicyCommand::Update => " FOR UPDATE",
        PolicyCommand::Delete => " FOR DELETE",
    });
    // `TO` 句の対象は PUBLIC のみ（名前付きロールは持てない）。省略時も PUBLIC。
    for r in &def.roles {
        match r {
            PolicyRole::Public => {}
        }
    }
    if let Some(u) = &def.using_sql {
        s.push_str(" USING (");
        s.push_str(&ctx.namer.externalize::<E>("policy", u)?);
        s.push(')');
    }
    if let Some(c) = &def.check_sql {
        s.push_str(" WITH CHECK (");
        s.push_str(&ctx.namer.externalize::<E>("policy", c)?);
        s.push(')');
    }
    Ok(s)
}

/// 選択したテナントの `CREATE POLICY` と行セキュリティの有効化・強制。
pub(super) fn export_row_security_and_policies<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    for key in tenant_table_keys::<E>(ctx)? {
        for def in list_policies_in_source(ctx.rt, tenant, &key)? {
            let sql = render_policy(ctx, &key, &def)?;
            out.emit(DdlSection::PostData, DdlObjectKind::Policy, &sql)?;
        }
        let state = row_security_state_in_txn(ctx.rt, tenant, &key)?;
        let table = ctx.namer.qualified_required::<E>(&key)?;
        if state.enabled {
            let sql = format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY");
            out.emit(DdlSection::PostData, DdlObjectKind::RowSecurity, &sql)?;
        }
        if state.forced {
            let sql = format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY");
            out.emit(DdlSection::PostData, DdlObjectKind::RowSecurity, &sql)?;
        }
    }
    Ok(())
}
