//! `CREATE [OR REPLACE] FUNCTION`／`DROP FUNCTION [IF EXISTS]` の永続化経路（SQL-66・TASK-309、
//! Issue #1920・#1917。親: #1918。設計記録: `docs/design/udf-catalog.md`「書き込み経路」）。
//!
//! 役割: セッションの [`UdfRegistry`] だけにあった UDF 定義を UDF カタログ（`catalog::function`）へ
//! 書く・消す実行本体と、[`CatalogError`] → SQLSTATE の写像を担う。呼び出し元は `core.rs` の
//! `execute_validated_in_session`（`CREATE FUNCTION`）と `ParsedSql::DropFunction` 腕で、どちらも
//! `require_ddl_permission` を通った場合だけここへ来る（権限なしは `42501`・#1922）。
//!
//! 順序の契約: 検証（レジストリ不変）→ カタログの単一 write txn・1 回の commit → commit 成功後に
//! レジストリへ反映。カタログで失敗したらセッションは変わらない。クラッシュしても commit 境界で
//! 全か無かになる（`recovery::commit_boundary` 経由）。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントの関数は不在と
//! 同じ経路・同じ `42883` になる。エラー文言は固定で、内部キー（`#<id>.<name>`）・tenant_id・本体の
//! 原文を含めない。

use crate::catalog::function::{
    FunctionAttributes, FunctionDef, FunctionLanguage, FunctionParam, FunctionType, Parallel,
    Volatility, FUNCTION_ALREADY_EXISTS_MESSAGE, FUNCTION_AMBIGUOUS_MESSAGE,
    FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE, FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE,
    FUNCTION_RECURSIVE_MESSAGE,
};
use crate::catalog::CatalogError;
use crate::sql::allowlist::{
    DropFunctionBehavior, FunctionAttributeClauses, FunctionLanguageClause, FunctionParallelClause,
    FunctionVolatilityClause, SqlSurfaceError, ValidatedDropFunction,
};
use crate::sql::cast::CastType;
use crate::sql::mode::SessionState;
use crate::sql::udf_call::{
    body_reaches_function, commit_function_definition, prepare_function_replacement,
    prepare_function_with_signature_prechecked, PreparedUdf,
};
use crate::storage::Storage;

/// `DROP FUNCTION` の成功応答。結果行は持たず、wire はコマンドタグ `DROP FUNCTION` を返す。
///
/// **Issue #1917 で追加した破壊的変更（BREAKING CHANGE）**: `IF EXISTS` で対象が無く何も消さなかった
/// ことを表す `skipped` を追加した（外部から `{}` で構築できなくなる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropFunctionOutcome {
    pub(crate) skipped: bool,
}

impl DropFunctionOutcome {
    /// `IF EXISTS` で対象が無く、何も削除しなかったか。他テナントの定義・存在しない定義はどちらも
    /// `true` になり、存在情報を区別できない（P0）。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// UDF シグネチャの型（[`CastType`]）をカタログの保存値へ写す（Issue #2798。SQL-66）。
/// 作成時の検証（`expr_type()` が `Some`）を通った型だけが来る前提で、写せなければ固定文言の
/// `0A000`（fail-closed）。修飾子なしの `numeric` は `result_column_type` が返す正規の
/// `numeric(38,16)` として保存し、ロード側の [`udf_cast_from_function_type`] が `Numeric(None)` へ戻す。
pub(crate) fn function_type_from_udf_cast(ty: &CastType) -> Result<FunctionType, SqlSurfaceError> {
    let unsupported =
        || SqlSurfaceError::feature_not_supported("function type cannot be persisted");
    if ty.expr_type().is_none() {
        return Err(unsupported());
    }
    let column = ty.result_column_type().ok_or_else(unsupported)?;
    FunctionType::from_column_type(&column).map_err(|_| unsupported())
}

/// カタログの保存値を UDF シグネチャの型へ戻す（ロード経路用。Issue #2798）。
/// 書き込み経路が作る正規形（[`function_type_from_udf_cast`] の出力）に往復一致する値だけを `Some` にし、
/// それ以外（`varchar(n)`・`numeric(p,s)`・`bpchar`・`vector`・ENUM・式型の無い配列など）は黙って補正せず
/// `None` とする。呼び出し側はその定義だけを skip する（fail-closed）。
pub(crate) fn udf_cast_from_function_type(ft: &FunctionType) -> Option<CastType> {
    let column = ft.to_column_type().ok()?;
    let cast = match crate::sql::cast::cast_type_of_column(&column)? {
        CastType::Numeric(Some(_)) => CastType::Numeric(None),
        // 構文段が `varchar(n)` を修飾子なしへ正規化するため、長さつきの行は本番経路では書かれない。
        CastType::Varchar(Some(_)) | CastType::Bpchar(_) => return None,
        other => other,
    };
    (function_type_from_udf_cast(&cast).ok().as_ref() == Some(ft)).then_some(cast)
}

/// カタログへ書く [`FunctionDef`] を検証済みの定義から組み立てる（`CREATE`・`OR REPLACE` 共通）。
/// 型注釈・RETURNS は保存値へ写す（Issue #2798）。
fn catalog_def_from_prepared(
    prepared: &PreparedUdf,
    body_source: &str,
    attrs: &FunctionAttributes,
) -> Result<FunctionDef, SqlSurfaceError> {
    let params = prepared
        .params
        .iter()
        .zip(prepared.param_types.iter())
        .map(|(p, ty)| {
            Ok(FunctionParam {
                name: Some(p.clone()),
                ty: ty.as_ref().map(function_type_from_udf_cast).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, SqlSurfaceError>>()?;
    Ok(FunctionDef {
        id: 1,
        function_key: prepared.key.clone(),
        params,
        return_type: prepared
            .return_type
            .as_ref()
            .map(function_type_from_udf_cast)
            .transpose()?,
        language: FunctionLanguage::Declarative,
        body_source: body_source.to_string(),
        attrs: attrs.clone(),
    })
}

/// 構文段の属性句（[`FunctionAttributeClauses`]）をカタログの保存値へ写す（Issue #2217・SQL-67 (3)）。
/// 未指定の項目はカタログ既定（`VOLATILE`・`CALLED ON NULL INPUT`・`PARALLEL UNSAFE`）になる。
/// `SECURITY`・`ROWS` は構文段が `SECURITY INVOKER` 以外・集合返却なしの `ROWS` を拒否済みで、ここへ来ない。
pub(crate) fn function_attributes_from_clauses(
    clauses: &FunctionAttributeClauses,
) -> FunctionAttributes {
    let mut attrs = FunctionAttributes::default();
    if let Some(v) = clauses.volatility() {
        attrs.volatility = match v {
            FunctionVolatilityClause::Immutable => Volatility::Immutable,
            FunctionVolatilityClause::Stable => Volatility::Stable,
            FunctionVolatilityClause::Volatile => Volatility::Volatile,
        };
    }
    attrs.strict = clauses.strict().unwrap_or(false);
    if let Some(v) = clauses.parallel() {
        attrs.parallel = match v {
            FunctionParallelClause::Unsafe => Parallel::Unsafe,
            FunctionParallelClause::Restricted => Parallel::Restricted,
            FunctionParallelClause::Safe => Parallel::Safe,
        };
    }
    attrs.cost = clauses.cost();
    attrs
}

/// 永続定義がセッション限定の関数（カタログ由来でない宣言的 UDF・WASM UDF）へ直接・推移的に依存する
/// 場合は `0A000` で拒否する。永続定義は別接続・再起動後にその呼び出し先が無く、ロードで読み飛ばされて
/// 呼べなくなるため（Issue #1922）。`0A000` は同じ「現状では永続化できない形」の既存拒否
/// （明示トランザクション内の永続化など）と同じ契約。エラー文言に関数名は含めない。
fn reject_session_local_dependency(
    session: &SessionState,
    body: &crate::sql::udf_call::Expr,
) -> Result<(), SqlSurfaceError> {
    if crate::sql::udf_call::body_depends_on_session_local(body, session.udfs()) {
        return Err(SqlSurfaceError::feature_not_supported(
            "a persistent function cannot depend on a session-local function",
        ));
    }
    Ok(())
}

/// 本体検証より前のカタログ側の重複・件数の先行判定（[`Storage::precheck_function_capacity`]）を
/// `SqlSurfaceError` へ写す。`CREATE`／`CREATE OR REPLACE` の永続化が共有し、判定順の実装を 1 箇所に
/// 保つ（重複 `42723`・件数 `54000` の写像は書き込み時の再検査と同じ [`map_function_error`]）。
fn precheck_catalog_capacity(
    storage: &Storage,
    tenant_id: &str,
    name: &str,
    max_functions: usize,
    replace: bool,
) -> Result<(), SqlSurfaceError> {
    storage
        .precheck_function_capacity(tenant_id, name, max_functions, replace)
        .map_err(map_function_error)
}

/// `CREATE FUNCTION` をカタログへ永続化してセッションへ登録する（DDL 権限のあるセッション用）。
///
/// `max_functions` は起動時設定の件数上限（Issue #1922。ハード上限で飽和される）。
///
/// 判定順は 予約名・識別子 → セッションの重複 `42723` → カタログの重複 `42723`・件数上限 `54000`
/// （先行判定）→ 引数数・閉じた本体 → [`Storage::create_function_with_limit`]（同じ重複・件数の
/// 排他的な再検査）→ セッションへの登録。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_create_function_persistent(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    name: &str,
    params: &[String],
    param_types: &[Option<CastType>],
    return_type: Option<&CastType>,
    body: &crate::sql::udf_call::Expr,
    body_source: &str,
    max_functions: usize,
    attrs: &FunctionAttributes,
) -> Result<(), SqlSurfaceError> {
    // 判定順（設計文書）は 重複 → 件数 → 引数数 → 本体。カタログ側の重複・件数は本体検証より前に
    // 先行判定し、書き込み時の排他的な再検査（`create_function_with_limit`）は権威として残す。
    let prepared = prepare_function_with_signature_prechecked(
        session.udfs(),
        name,
        params,
        param_types,
        return_type,
        body,
        &|| precheck_catalog_capacity(storage, tenant_id, name, max_functions, false),
    )?;
    reject_session_local_dependency(session, &prepared.body)?;
    let def = catalog_def_from_prepared(&prepared, body_source, attrs)?;
    storage
        .create_function_with_limit(tenant_id, &def, max_functions)
        .map_err(map_function_error)?;
    let key = prepared.key.clone();
    commit_function_definition(session.udfs_mut(), prepared);
    // カタログに永続化した定義はカタログ由来として印付けする（他セッションの DROP を refresh が
    // 反映するときの入れ替え対象・権限の無いセッションの DROP の判定に使う。#1921）。
    session.udfs_mut().mark_catalog_origin(&key);
    Ok(())
}

/// `CREATE OR REPLACE FUNCTION` をカタログへ永続化してセッションへ反映する（DDL 権限のあるセッション用。
/// SQL-66 (2)・Issue #1917）。順序は [`prepare_function_replacement`]（レジストリ不変。シグネチャ・引数名・
/// 戻り値型・循環の検査）→ [`Storage::replace_function`]（同名が無ければ作成、あれば同じ内部 ID のまま
/// 置換。単一 write txn・1 回の commit）→ commit 成功後にレジストリを上書きしてカタログ由来の印を付け直す。
/// カタログで失敗したらセッションは変わらない。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_create_or_replace_function_persistent(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    name: &str,
    params: &[String],
    param_types: &[Option<CastType>],
    return_type: Option<&CastType>,
    body: &crate::sql::udf_call::Expr,
    body_source: &str,
    max_functions: usize,
    attrs: &FunctionAttributes,
) -> Result<(), SqlSurfaceError> {
    // 置換（カタログに同名の定義がある）は件数を増やさないため先行の件数判定を省く。新規作成になる
    // ときだけ本体検証より前に件数を判定する（write txn 内の再検査は権威として残す）。
    let prepared = prepare_function_replacement(
        session.udfs(),
        name,
        params,
        param_types,
        return_type,
        body,
        &|| precheck_catalog_capacity(storage, tenant_id, name, max_functions, true),
    )?;
    reject_session_local_dependency(session, &prepared.body)?;
    let def = catalog_def_from_prepared(&prepared, body_source, attrs)?;
    // 循環検査はセッションのレジストリ（古い可能性がある）ではなく、write txn 内の最新カタログで
    // 再度行う（並行する別セッションの置換との合成で循環が残るのを防ぐ）。
    let target = prepared.key.to_ascii_lowercase();
    let new_body = &prepared.body;
    storage
        .replace_function_checked(tenant_id, &def, max_functions, &|latest| {
            let mut bodies = std::collections::BTreeMap::new();
            for d in latest {
                let lower = d.function_key.to_ascii_lowercase();
                if lower == target || d.language != FunctionLanguage::Declarative {
                    continue;
                }
                let Ok(tokens) = crate::sql::lexer::tokenize_rendered(&d.body_source) else {
                    continue;
                };
                let Ok(body) = crate::sql::allowlist::parse_stored_function_body(&tokens) else {
                    continue;
                };
                bodies.insert(lower, body);
            }
            if body_reaches_function(new_body, &target, &bodies) {
                return Err(CatalogError::Invalid(
                    FUNCTION_RECURSIVE_MESSAGE.to_string(),
                ));
            }
            Ok(())
        })
        .map_err(map_function_error)?;
    let key = prepared.key.clone();
    commit_function_definition(session.udfs_mut(), prepared);
    session.udfs_mut().mark_catalog_origin(&key);
    Ok(())
}

/// `LANGUAGE sql|plpgsql` の関数 1 件の定義入力（[`execute_create_routine_persistent`] の引数束）。
/// 本体は原文のまま保存し、解析・束縛・実行は呼び出し時の経路（`core::routine_call`。#2218）が担う。
pub(crate) struct RoutineSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) params: &'a [String],
    pub(crate) param_types: &'a [Option<CastType>],
    pub(crate) return_type: Option<&'a CastType>,
    pub(crate) returns_trigger: bool,
    pub(crate) language: FunctionLanguageClause,
    pub(crate) attrs: FunctionAttributes,
    pub(crate) body_source: &'a str,
}

/// `CREATE [OR REPLACE] FUNCTION ... LANGUAGE sql|plpgsql` をカタログへ永続化する（DDL 権限のある
/// セッション用。Issue #2217・SQL-67 (3)）。呼び出し元は `core.rs` の `CreateFunction` 腕で、
/// `require_ddl_permission` の後にだけ来る。
///
/// 判定順は 予約名・識別子 → セッションの同名衝突 `42723` → カタログの重複 `42723`・件数 `54000`
/// （先行判定）→ 引数名 → 本体検証 `validate`（sql・plpgsql の定義時検証）→ カタログの単一 write txn。
/// commit 後に同じセッションの [`UdfRegistry`] へ載せる（呼び出しの実行は `core::routine_call`。#2218）。
/// OR REPLACE が宣言的 UDF を置き換えた場合は、commit 後にセッションの旧本体の展開を取り除く。
///
/// [`UdfRegistry`]: crate::sql::udf_call::UdfRegistry
pub(crate) fn execute_create_routine_persistent(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    spec: &RoutineSpec<'_>,
    or_replace: bool,
    max_functions: usize,
    validate: &dyn Fn() -> Result<(), SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    let name = spec.name;
    crate::catalog::validate_relation_key(name)
        .map_err(|_| SqlSurfaceError::invalid_input("invalid function name"))?;
    let lower = name.to_ascii_lowercase();
    if crate::sql::udf_call::is_reserved_function_name(name) {
        return Err(SqlSurfaceError::duplicate_function(
            "function name collides with a built-in or reserved name",
        ));
    }
    // セッションの WASM UDF とは名前空間を共有する。CREATE は宣言的 UDF の同名も衝突、OR REPLACE は
    // 宣言的 UDF の置換だけ許す（commit 後に取り除く）。
    let session_wasm = session.udfs().get_wasm(name).is_some();
    if session_wasm || (!or_replace && session.udfs().is_name_taken(&lower)) {
        return Err(SqlSurfaceError::duplicate_function(
            "function is already defined in this session",
        ));
    }
    precheck_catalog_capacity(storage, tenant_id, name, max_functions, or_replace)?;
    if spec.params.len() > crate::sql::udf_call::MAX_UDF_PARAMS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many function parameters",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut params = Vec::with_capacity(spec.params.len());
    for (p, ty) in spec.params.iter().zip(spec.param_types.iter()) {
        crate::catalog::validate_identifier(p)
            .map_err(|_| SqlSurfaceError::invalid_input("invalid parameter name"))?;
        let normalized = p.to_ascii_lowercase();
        if !seen.insert(normalized.clone()) {
            return Err(SqlSurfaceError::invalid_input("duplicate parameter name"));
        }
        let ty = ty
            .as_ref()
            .ok_or_else(|| SqlSurfaceError::unsupported("function parameters must be typed"))?;
        params.push(FunctionParam {
            name: Some(normalized),
            ty: Some(function_type_from_udf_cast(ty)?),
        });
    }
    let return_type = if spec.returns_trigger {
        FunctionType::trigger()
    } else {
        let ty = spec.return_type.ok_or_else(|| {
            SqlSurfaceError::invalid_function_definition("function result type must be specified")
        })?;
        function_type_from_udf_cast(ty)?
    };
    validate()?;
    let def = FunctionDef {
        id: 1,
        function_key: lower.clone(),
        params,
        return_type: Some(return_type),
        language: match spec.language {
            FunctionLanguageClause::Sql => FunctionLanguage::Sql,
            FunctionLanguageClause::Plpgsql => FunctionLanguage::Plpgsql,
            FunctionLanguageClause::Declarative => {
                return Err(SqlSurfaceError::Internal {
                    detail: "declarative function routed as routine".to_string(),
                })
            }
        },
        body_source: spec.body_source.to_string(),
        attrs: spec.attrs.clone(),
    };
    if or_replace {
        // 非宣言的な新定義は別の定義を呼ばないため循環検査は常に成功する。
        storage
            .replace_function_checked(tenant_id, &def, max_functions, &|_| Ok(()))
            .map_err(map_function_error)?;
        // 宣言的 UDF を置き換えた場合、セッションに旧本体の展開が残らないよう取り除く。
        session.udfs_mut().remove(name);
    } else {
        storage
            .create_function_with_limit(tenant_id, &def, max_functions)
            .map_err(map_function_error)?;
    }
    // commit 後に同じセッションへ登録する（すぐ呼べるようにする。他セッションは世代で refresh）。
    // 非カタログ登録・WASM・容量で入れられない場合は登録せず、次の refresh のロードに任せる。
    if let Some(def) = routine_from_spec(spec) {
        let session_udfs = session.udfs_mut();
        session_udfs.remove(name);
        if session_udfs.len() < crate::sql::udf_call::MAX_SESSION_UDFS {
            session_udfs.insert_routine(name, def);
            session_udfs.mark_catalog_origin(name);
        }
    }
    Ok(())
}

/// 関数の定義入力から、セッションのレジストリへ載せるルーチン定義を作る（`RETURNS` が無い・型が
/// 写せない場合は `None`＝登録しない）。
fn routine_from_spec(spec: &RoutineSpec<'_>) -> Option<crate::sql::udf_call::RoutineDefinition> {
    // カタログから再ロードしたときと同じ型になるよう、保存値への往復で正規化する（往復一致しない型の
    // 関数はロード側でも skip されるため、同じセッションにも載せない）。
    let normalize = |t: &CastType| {
        function_type_from_udf_cast(t)
            .ok()
            .and_then(|ft| udf_cast_from_function_type(&ft))
    };
    let param_types = spec
        .param_types
        .iter()
        .map(|t| t.as_ref().and_then(normalize))
        .collect::<Option<Vec<CastType>>>()?;
    let ret = if spec.returns_trigger {
        None
    } else {
        Some(normalize(spec.return_type?)?)
    };
    Some(crate::sql::udf_call::RoutineDefinition {
        params: spec.params.iter().map(|p| p.to_ascii_lowercase()).collect(),
        param_types,
        ret,
        language: match spec.language {
            FunctionLanguageClause::Sql => FunctionLanguage::Sql,
            FunctionLanguageClause::Plpgsql => FunctionLanguage::Plpgsql,
            FunctionLanguageClause::Declarative => return None,
        },
        attrs: spec.attrs.clone(),
        body_source: spec.body_source.to_string(),
    })
}

/// `DROP FUNCTION [IF EXISTS] ... [CASCADE | RESTRICT]` を実行する。DDL 権限の無いセッションは
/// 何も見ず `42501`（`IF EXISTS` でも同じ。存在情報を出さない）。権限のあるセッションはカタログ
/// （自テナントの scope）から削除し、レジストリからも削除する（カタログに載らない定義、たとえば WASM UDF は
/// レジストリからだけ消える）。
/// **どちらでも消えなかった場合だけ** `42883`（存在情報を出さない固定文言）。`IF EXISTS` なら代わりに
/// `skipped` の成功を返す（他テナントの定義・不在はどちらも同じ応答）。
/// 依存トリガは `RESTRICT`（既定）なら `2BP01`、`CASCADE` なら関数と同じ write txn で連鎖削除する。
///
/// 型の並びが指定されたときは、カタログ上の定義の引数の数と（型注釈のある引数の）型が一致する
/// 場合だけ削除する（型注釈なしの引数は任意の型に一致する）。不一致は不在と同じ `42883`。
pub(crate) fn execute_drop_function(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    stmt: &ValidatedDropFunction,
) -> Result<DropFunctionOutcome, SqlSurfaceError> {
    // 権限検査は保存先（カタログ／セッション内）の振り分けより前。`IF EXISTS` でも `42501` にして、
    // 権限の無いセッションに対象の有無を区別させない（SQL-66・Issue #1922・fail-closed）。
    crate::sql::ddl::require_ddl_permission(session)?;
    let mut removed = false;
    removed |= drop_from_catalog(storage, tenant_id, stmt)?;
    // 型の並びがある場合、セッション内の定義は引数の数と（型注釈のある引数の）型で照合する。
    // WASM UDF は (vector, scalar) の固定 2 引数なので、同じ名前空間を共有する
    // `UdfRegistry::remove` が両表を消す設計に合わせて判定にも含める。
    let session_match = {
        let udfs = session.udfs();
        let arity_ok = |n: usize| stmt.arg_types.as_ref().is_none_or(|types| types.len() == n);
        udfs.get(&stmt.name)
            .is_some_and(|d| arity_ok(d.params.len()) && session_types_match(d, stmt))
            || udfs.get_wasm(&stmt.name).is_some_and(|_| arity_ok(2))
            || udfs.get_routine(&stmt.name).is_some_and(|r| {
                arity_ok(r.params.len()) && routine_types_match(&r.param_types, stmt)
            })
    };
    if session_match {
        removed |= session.udfs_mut().remove(&stmt.name);
    }
    if removed {
        Ok(DropFunctionOutcome { skipped: false })
    } else if stmt.if_exists {
        Ok(DropFunctionOutcome { skipped: true })
    } else {
        Err(SqlSurfaceError::undefined_function(
            "function does not exist",
        ))
    }
}

/// セッション内の定義の型注釈が `DROP FUNCTION` の型の並びと一致するか（Issue #2798）。カタログ側の
/// 照合（[`drop_from_catalog`]）と同じ規則: 型の並びが無い・型注釈なしの引数は任意の型に一致し、
/// 型注釈のある引数は同一性トークン（typmod を落とした形）で比べる。不一致は不在と同じ扱いにするため、
/// カタログに無い定義だけがレジストリから消えて成功応答になる食い違いを防ぐ。
fn session_types_match(
    def: &crate::sql::udf_call::UdfDefinition,
    stmt: &ValidatedDropFunction,
) -> bool {
    let Some(types) = &stmt.arg_types else {
        return true;
    };
    def.param_types.iter().zip(types).all(|(declared, want)| {
        let Some(declared) = declared else {
            return true;
        };
        let (Ok(have), Ok(want)) = (
            function_type_from_udf_cast(declared),
            FunctionType::from_column_type(want),
        ) else {
            return false;
        };
        have.identity_token() == want.identity_token()
    })
}

/// セッション内のルーチンの引数型が `DROP FUNCTION` の型の並びと一致するか（[`session_types_match`] と
/// 同じ規則。ルーチンの引数は全て型注釈つき）。
fn routine_types_match(declared: &[CastType], stmt: &ValidatedDropFunction) -> bool {
    let Some(types) = &stmt.arg_types else {
        return true;
    };
    declared.iter().zip(types).all(|(declared, want)| {
        let (Ok(have), Ok(want)) = (
            function_type_from_udf_cast(declared),
            FunctionType::from_column_type(want),
        ) else {
            return false;
        };
        have.identity_token() == want.identity_token()
    })
}

/// カタログから削除し、削除したら `true`。型付きの場合も照合と削除は同一 write txn で行う。
fn drop_from_catalog(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedDropFunction,
) -> Result<bool, SqlSurfaceError> {
    let cascade = stmt.behavior == DropFunctionBehavior::Cascade;
    let Some(types) = &stmt.arg_types else {
        return storage
            .drop_function_by_name(tenant_id, &stmt.name, cascade)
            .map(|sig| sig.is_some())
            .map_err(map_function_error);
    };
    // 列挙・署名照合・削除を 1 つの write txn で行う（読みと削除の間に別セッションの
    // drop／再作成が割り込んで新しい定義を誤って消さないため）。
    storage
        .drop_function_matching(tenant_id, &stmt.name, cascade, |def| {
            if def.params.len() != types.len() {
                return Ok(false);
            }
            for (stored, want) in def.params.iter().zip(types) {
                let Some(stored_ty) = &stored.ty else {
                    continue;
                };
                let want_ty = FunctionType::from_column_type(want)?;
                if stored_ty.identity_token() != want_ty.identity_token() {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .map_err(map_function_error)
}

/// UDF カタログの [`CatalogError`] を SQL 表層の契約へ写像する（同名の既存 `42723`・件数上限
/// `54000`・ENUM 等の未対応 `0A000`・形状不正 `22000`・ロック timeout `55P03`・その他 `XX000`）。
/// 文言は固定で、内部キー・tenant_id・本体を含めない（security.md P0）。
fn map_function_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::Invalid(m) if m == FUNCTION_ALREADY_EXISTS_MESSAGE => {
            SqlSurfaceError::duplicate_function("function already exists")
        }
        // 作成時に同名の多重定義を拒否しているため通常は起きない防御（Issue #1917 で `42725` にした）。
        CatalogError::Invalid(m) if m == FUNCTION_AMBIGUOUS_MESSAGE => {
            SqlSurfaceError::ambiguous_function("function name is not unique")
        }
        // `CREATE OR REPLACE FUNCTION` が引数名・戻り値型を変えようとした（SQL-66 (2)・Issue #1917）。
        CatalogError::Invalid(m) if m == FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE => {
            SqlSurfaceError::invalid_function_definition("cannot change name of input parameter")
        }
        CatalogError::Invalid(m) if m == FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE => {
            SqlSurfaceError::invalid_function_definition(
                "cannot change return type of existing function",
            )
        }
        CatalogError::Invalid(m) if m == FUNCTION_RECURSIVE_MESSAGE => {
            SqlSurfaceError::feature_not_supported(
                "recursive function definitions are not supported",
            )
        }
        CatalogError::Invalid(m) if m.contains("enum") => {
            SqlSurfaceError::feature_not_supported("enum types are not supported in functions")
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid function definition"),
        CatalogError::ViewLimitExceeded(_) => {
            SqlSurfaceError::payload_too_large("too many functions")
        }
        // 依存する TRIGGER が残っている（`RESTRICT`。`2BP01`）。内部キー・トリガ名を出さない固定文言。
        CatalogError::DependentObjectsStillExist(_) => {
            SqlSurfaceError::DependentObjectsStillExist {
                name: "function".to_string(),
            }
        }
        // 名前解決後に SCHEMA が削除された（または他テナントの ID）。存在情報を出さない固定文言。
        CatalogError::TableNotFound(_) => {
            SqlSurfaceError::undefined_function("function schema does not exist")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "function catalog operation failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::{prepare_function_definition, prepare_function_with_signature};

    fn parse_body(src: &str) -> crate::sql::udf_call::Expr {
        let tokens = crate::sql::lexer::tokenize_rendered(src).expect("tokens");
        crate::sql::allowlist::parse_stored_function_body(&tokens).expect("body")
    }

    /// Issue #1917: 古いレジストリで循環検査を通っても、write txn 内の最新カタログで再検査され、
    /// 並行置換 f→g・g→f の合成による循環がカタログに残らない。
    #[test]
    fn replace_rechecks_cycle_against_latest_catalog() {
        let path = crate::test_util::temp_db::unique_db_path("udf-ddl-cycle");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        let mut a = SessionState::default();
        let mut b = SessionState::default();
        // 初期状態 f(x) AS x・g(x) AS x を両セッションが同じ状態として持つ。
        for sess in [&mut a, &mut b] {
            let _ = sess;
        }
        let p = vec!["x".to_string()];
        let body_x = parse_body("x");
        let mut seed = SessionState::default();
        let _ = &mut seed;
        let prepared_f = prepare_function_definition(a.udfs(), "f", &p, &body_x).expect("f");
        storage
            .create_function(
                "t",
                &catalog_def_from_prepared(&prepared_f, "x", &FunctionAttributes::default())
                    .expect("def"),
            )
            .expect("create f");
        commit_function_definition(a.udfs_mut(), prepared_f);
        // 実運用ではカタログ由来のロード・永続化が印を付ける（付けないとセッション限定の依存として拒否される）。
        a.udfs_mut().mark_catalog_origin("f");
        let prepared_g = prepare_function_definition(a.udfs(), "g", &p, &body_x).expect("g");
        storage
            .create_function(
                "t",
                &catalog_def_from_prepared(&prepared_g, "x", &FunctionAttributes::default())
                    .expect("def"),
            )
            .expect("create g");
        commit_function_definition(a.udfs_mut(), prepared_g);
        a.udfs_mut().mark_catalog_origin("g");
        for key in ["f", "g"] {
            let prep = prepare_function_definition(b.udfs(), key, &p, &body_x).expect("b");
            commit_function_definition(b.udfs_mut(), prep);
            b.udfs_mut().mark_catalog_origin(key);
        }
        execute_create_or_replace_function_persistent(
            &storage,
            "t",
            &mut a,
            "f",
            &p,
            &[None],
            None,
            &parse_body("g(x)"),
            "g ( x )",
            crate::catalog::function::MAX_FUNCTIONS,
            &FunctionAttributes::default(),
        )
        .expect("f -> g");
        // b のレジストリは f を単純な本体だと思っているため、レジストリ上の検査は通る。
        let err = execute_create_or_replace_function_persistent(
            &storage,
            "t",
            &mut b,
            "g",
            &p,
            &[None],
            None,
            &parse_body("f(x)"),
            "f ( x )",
            crate::catalog::function::MAX_FUNCTIONS,
            &FunctionAttributes::default(),
        )
        .expect_err("g -> f must be rejected by the catalog-level check");
        assert_eq!(err.wire_code(), "0A000");
    }

    /// Issue #1922: 件数上限に達したテナントでは、本体・引数数の検証より先に件数超過 `54000` を返す。
    /// 重複 `42723` は件数より先、置換（既存定義あり）は上限到達でも本体検証まで進む。
    #[test]
    fn limit_is_judged_before_body_and_param_count_validation() {
        let path = crate::test_util::temp_db::unique_db_path("udf-ddl-limit-order");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        let mut s = SessionState::default();
        let p = vec!["x".to_string()];
        let create = |s: &mut SessionState, name: &str, params: &[String], body: &str| {
            execute_create_function_persistent(
                &storage,
                "t",
                s,
                name,
                params,
                &vec![None; params.len()],
                None,
                &parse_body(body),
                body,
                1,
                &FunctionAttributes::default(),
            )
        };
        let replace = |s: &mut SessionState, name: &str, body: &str| {
            execute_create_or_replace_function_persistent(
                &storage,
                "t",
                s,
                name,
                &p,
                &[None],
                None,
                &parse_body(body),
                body,
                1,
                &FunctionAttributes::default(),
            )
        };
        create(&mut s, "f", &p, "x").expect("first function");
        // 未定義関数を呼ぶ本体・引数数超過のいずれも件数超過が先。
        let err = create(&mut s, "g", &p, "missing(x)").expect_err("limit");
        assert_eq!(err.wire_code(), "54000");
        let many: Vec<String> = (0..200).map(|i| format!("a{i}")).collect();
        let err = create(&mut s, "g", &many, "a0").expect_err("limit");
        assert_eq!(err.wire_code(), "54000");
        // 新規作成になる OR REPLACE も同じ。
        let err = replace(&mut s, "h", "missing(x)").expect_err("limit");
        assert_eq!(err.wire_code(), "54000");
        // 重複は件数より先。
        let err = create(&mut s, "f", &p, "x").expect_err("duplicate");
        assert_eq!(err.wire_code(), "42723");
        // 置換は上限到達でも本体検証まで進む（未定義関数は 42883、正しい本体は成功）。
        let err = replace(&mut s, "f", "missing(x)").expect_err("body");
        assert_eq!(err.wire_code(), "42883");
        replace(&mut s, "f", "x").expect("replace at the limit");
    }

    #[test]
    fn catalog_errors_map_to_fixed_sqlstates() {
        let dup = map_function_error(CatalogError::Invalid(
            FUNCTION_ALREADY_EXISTS_MESSAGE.to_string(),
        ));
        assert_eq!(dup.wire_code(), "42723");
        let limit = map_function_error(CatalogError::ViewLimitExceeded("x".to_string()));
        assert_eq!(limit.wire_code(), "54000");
        let enum_ty = map_function_error(CatalogError::Invalid(
            "enum type is not supported in function signatures".to_string(),
        ));
        assert_eq!(enum_ty.wire_code(), "0A000");
        let other = map_function_error(CatalogError::Invalid("bad".to_string()));
        assert_eq!(other.wire_code(), "22000");
        let ambiguous = map_function_error(CatalogError::Invalid(
            FUNCTION_AMBIGUOUS_MESSAGE.to_string(),
        ));
        assert_eq!(ambiguous.wire_code(), "42725");
        let rename = map_function_error(CatalogError::Invalid(
            FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE.to_string(),
        ));
        assert_eq!(rename.wire_code(), "42P13");
        let retype = map_function_error(CatalogError::Invalid(
            FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE.to_string(),
        ));
        assert_eq!(retype.wire_code(), "42P13");
        let dep = map_function_error(CatalogError::DependentObjectsStillExist(
            "#7.secret".to_string(),
        ));
        assert_eq!(dep.wire_code(), "2BP01");
        assert!(!dep.client_message().contains("secret"));
        let corrupt = map_function_error(CatalogError::CorruptSchema("secret #1.f".to_string()));
        assert_eq!(corrupt.wire_code(), "XX000");
        assert!(!corrupt.client_message().contains("secret"));
    }

    /// SQL-66・#2798: UDF シグネチャで受理される型は保存値を経て元の型へ往復一致する
    /// （修飾子なしの numeric は正規の `numeric(38,16)` で保存され `Numeric(None)` へ戻る）。
    #[test]
    fn udf_signature_types_round_trip_through_catalog_values() {
        let types = [
            CastType::Int2,
            CastType::Int4,
            CastType::Int8,
            CastType::Float4,
            CastType::Float8,
            CastType::Numeric(None),
            CastType::Bool,
            CastType::Text,
            CastType::Varchar(None),
            CastType::Date,
            CastType::Time,
            CastType::Timestamp,
            CastType::TimestampTz,
            CastType::Interval,
            CastType::Json,
            CastType::Jsonb,
            CastType::Uuid,
            CastType::Bytea,
            CastType::Array(Box::new(CastType::Int4)),
            CastType::Array(Box::new(CastType::Text)),
        ];
        for ty in types {
            let ft = function_type_from_udf_cast(&ty).unwrap_or_else(|e| panic!("{ty:?}: {e}"));
            assert_eq!(udf_cast_from_function_type(&ft), Some(ty.clone()), "{ty:?}");
        }
    }

    /// SQL-66・#2798: 正規形に往復しない保存値は黙って補正せず `None`（呼び出し側がその定義を skip する）。
    #[test]
    fn non_canonical_stored_types_are_not_restored() {
        use crate::catalog::ColumnType;
        let cols = [
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            ColumnType::Varchar(Some(5)),
            ColumnType::Bpchar(3),
        ];
        for c in cols {
            let ft = FunctionType::from_column_type(&c).expect("ft");
            assert_eq!(udf_cast_from_function_type(&ft), None, "{c:?}");
        }
    }

    /// SQL-66・#2798: 型付き・RETURNS つきの定義がカタログの保存値へ写る。
    #[test]
    fn catalog_def_carries_param_and_return_types() {
        let reg = crate::sql::udf_call::UdfRegistry::default();
        let prepared = prepare_function_with_signature(
            &reg,
            "f",
            &["x".to_string()],
            &[Some(CastType::Int4)],
            Some(&CastType::Int8),
            &parse_body("x + 1"),
        )
        .expect("prepare");
        let def = catalog_def_from_prepared(&prepared, "x + 1", &FunctionAttributes::default())
            .expect("def");
        assert_eq!(
            def.params[0].ty.as_ref().map(FunctionType::identity_token),
            Some("integer".to_string())
        );
        assert_eq!(
            def.return_type.as_ref().map(FunctionType::identity_token),
            Some("bigint".to_string())
        );
    }
}
