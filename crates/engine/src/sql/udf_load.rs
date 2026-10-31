//! UDF カタログからセッションの [`UdfRegistry`] へのロード・refresh（SQL-66・TASK-309、Issue #1921。
//! 親: #1918。設計記録: `docs/design/udf-catalog.md`「ロード経路」）。
//!
//! 役割: `catalog::function` に永続化された宣言的 UDF を、認証済みテナントの scope に限って読み、
//! 本体を許可リストパーサーで再パースし、作成時と同じ検証（予約名・閉じた式・上限）を通して
//! セッションのレジストリへ登録する。呼び出し元は `core.rs` の `load_session_udfs`（wire-server の
//! ハンドシェイクが認証後に 1 回）と `refresh_session_udfs`（Query／Parse の前）。
//!
//! fail-closed の方針は 2 層に分ける。
//! - カタログ全体が読めない（blob 破損・I/O）: [`load`] が `Err`（`XX000`・固定文言）を返し、
//!   ハンドシェイクは接続を拒否する。refresh では stale な登録を残さずカタログ由来を全部消す。
//! - 1 件の定義が登録できない（再パース失敗・予約名との衝突・呼び出し先の消失・レジストリで表せない
//!   形）: その定義だけを登録しない（呼ぶと `42883`）。行はカタログに残るため DDL セッションが
//!   `DROP FUNCTION` で消せる。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す認証済み `tenant_id` だけ。レジストリに束縛した scope と
//! 異なるテナントでの呼び出しは、カタログ由来を全部消して何もロードしない。エラー・skip の経路は
//! tenant_id・内部キー・本体の原文を出さない。

use crate::catalog::function::{FunctionDef, FunctionLanguage, ReturnShape, MAX_FUNCTIONS};
use crate::sql::allowlist::{parse_stored_function_body, SqlSurfaceError};
use crate::sql::cast::CastType;
use crate::sql::udf_call::{
    commit_function_definition, prepare_function_with_signature,
    prepare_function_with_signature_ignoring_capacity, MAX_SESSION_UDFS,
};
use crate::sql::udf_call::{Expr, RoutineDefinition, RoutineReturn, UdfRegistry};
use crate::sql::udf_ddl::udf_cast_from_function_type;
use crate::storage::Storage;

/// refresh に失敗したセッションが次回必ず再試行するための世代（実在の世代と一致しない値）。
const STALE_GENERATION: u64 = u64::MAX;

/// ロード失敗の固定エラー（tenant_id・内部キー・blob の断片を含めない）。
fn load_failed() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "could not load function catalog".to_string(),
    }
}

/// 登録待ちへ復元した宣言的 UDF のシグネチャ（引数名・引数型・戻り値型）。
type Signature = (Vec<String>, Vec<Option<CastType>>, Option<CastType>);

/// レジストリ（宣言的 UDF）で表せる形か。表せれば引数名・型注釈・戻り値型を復元して返す
/// （Issue #2798。書き込み経路が作る正規形の型だけを受理し、`udf_cast_from_function_type` で
/// 往復一致しない型・型注釈の混在・型なし引数つきの RETURNS・宣言的でない言語の行は `None`＝その
/// 定義だけ skip する。構文段と同じ不変条件）。
fn representable(def: &FunctionDef) -> Option<Signature> {
    if def.language != FunctionLanguage::Declarative {
        return None;
    }
    let names = def
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect::<Option<Vec<String>>>()?;
    let types = def
        .params
        .iter()
        .map(|p| match &p.ty {
            None => Some(None),
            Some(ft) => udf_cast_from_function_type(ft).map(Some),
        })
        .collect::<Option<Vec<Option<CastType>>>>()?;
    let typed = types.iter().filter(|t| t.is_some()).count();
    if typed != 0 && typed != types.len() {
        return None;
    }
    let ret = match &def.return_type {
        None => None,
        Some(ft) => {
            if typed != types.len() {
                return None;
            }
            Some(udf_cast_from_function_type(ft)?)
        }
    };
    Some((names, types, ret))
}

/// `LANGUAGE sql|plpgsql` の行をルーチン定義へ復元できるか（Issue #2218。全引数が名前付き・型付きで、
/// 型が往復一致し、`RETURNS` がある行だけ。表せない行はその定義だけ skip する）。
fn representable_routine(def: &FunctionDef) -> Option<RoutineDefinition> {
    if !matches!(
        def.language,
        FunctionLanguage::Sql | FunctionLanguage::Plpgsql
    ) {
        return None;
    }
    let params = def
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect::<Option<Vec<String>>>()?;
    let param_types = def
        .params
        .iter()
        .map(|p| p.ty.as_ref().and_then(udf_cast_from_function_type))
        .collect::<Option<Vec<CastType>>>()?;
    // 戻り値の形を先に見る（`RETURNS TABLE` は `return_type` を持たない。Issue #2220）。
    let ret = match &def.return_shape {
        ReturnShape::Table(cols) => RoutineReturn::Set(
            cols.iter()
                .map(|(n, t)| Some((Some(n.clone()), udf_cast_from_function_type(t)?)))
                .collect::<Option<Vec<_>>>()?,
        ),
        ReturnShape::SetOf => RoutineReturn::Set(vec![(
            None,
            udf_cast_from_function_type(def.return_type.as_ref()?)?,
        )]),
        ReturnShape::Scalar => {
            let ft = def.return_type.as_ref()?;
            if ft.is_trigger() {
                RoutineReturn::Trigger
            } else {
                RoutineReturn::Scalar(udf_cast_from_function_type(ft)?)
            }
        }
    };
    Some(RoutineDefinition {
        params,
        param_types,
        ret,
        language: def.language,
        attrs: def.attrs.clone(),
        body_source: def.body_source.clone(),
    })
}

/// 再パース済みの登録待ち定義。
struct Pending {
    key: String,
    params: Vec<String>,
    param_types: Vec<Option<CastType>>,
    return_type: Option<CastType>,
    body: Expr,
}

/// テナント `tenant_id` の UDF カタログを `registry` へロードする（セッション開始時）。何度呼んでも
/// 安全（カタログ由来の登録を消してから入れ直す）。カタログ全体が読めなければ `Err`。
pub(crate) fn load(
    storage: &Storage,
    tenant_id: &str,
    registry: &mut UdfRegistry,
) -> Result<(), SqlSurfaceError> {
    if registry
        .catalog_scope()
        .is_some_and(|bound| bound != tenant_id)
    {
        registry.remove_catalog_entries();
        return Err(load_failed());
    }
    load_into(storage, tenant_id, registry)
}

/// 世代が変わっていれば再ロードする（Query／Parse の前）。未ロードのセッションは何もしない。
/// 失敗したらカタログ由来を全部消し、次回再試行する（stale な定義で実行し続けない）。
pub(crate) fn refresh(storage: &Storage, tenant_id: &str, registry: &mut UdfRegistry) {
    let Some(bound) = registry.catalog_scope() else {
        return;
    };
    if bound != tenant_id {
        registry.remove_catalog_entries();
        return;
    }
    if storage.function_generation(tenant_id) == registry.catalog_generation() {
        return;
    }
    if load_into(storage, tenant_id, registry).is_err() {
        registry.remove_catalog_entries();
        registry.set_catalog_binding(Some(tenant_id), STALE_GENERATION);
    }
}

fn load_into(
    storage: &Storage,
    tenant_id: &str,
    registry: &mut UdfRegistry,
) -> Result<(), SqlSurfaceError> {
    registry.remove_catalog_entries();
    // 世代を先に読む（読み取りの後に別セッションが変更しても、次の確認で必ずリロードされる順序）。
    let generation = storage.function_generation(tenant_id);
    let mut defs = storage
        .list_functions(tenant_id)
        .map_err(|_| load_failed())?;
    defs.sort_by_key(|d| d.id);

    let mut pending: Vec<Pending> = Vec::new();
    let mut routines: Vec<(String, RoutineDefinition)> = Vec::new();
    for def in defs {
        if let Some(r) = representable_routine(&def) {
            routines.push((def.function_key, r));
            continue;
        }
        let Some((params, param_types, return_type)) = representable(&def) else {
            continue;
        };
        let Ok(tokens) = crate::sql::lexer::tokenize_rendered(&def.body_source) else {
            continue;
        };
        let Ok(body) = parse_stored_function_body(&tokens) else {
            continue;
        };
        pending.push(Pending {
            key: def.function_key,
            params,
            param_types,
            return_type,
            body,
        });
    }

    // 呼び出し先が先に登録済みである必要があるため、id 昇順で試し、失敗した定義は次のパスへ回す
    // （呼び出し先を DROP して作り直すと id の順序が逆転する）。進捗が無くなれば終了し、残りは skip。
    for _ in 0..MAX_FUNCTIONS {
        if pending.is_empty() {
            break;
        }
        let mut progressed = false;
        let mut retry = Vec::new();
        for p in pending {
            match prepare_function_with_signature(
                registry,
                &p.key,
                &p.params,
                &p.param_types,
                p.return_type.as_ref(),
                &p.body,
            ) {
                Ok(prepared) => {
                    let key = prepared.key.clone();
                    commit_function_definition(registry, prepared);
                    registry.mark_catalog_origin(&key);
                    progressed = true;
                }
                Err(_) => retry.push(p),
            }
        }
        pending = retry;
        if !progressed {
            break;
        }
    }
    // `LANGUAGE sql|plpgsql` の関数（Issue #2218）。呼び出し先を持たないため宣言的 UDF の後に 1 パスで
    // 登録する。予約名・名前衝突・上限で登録できない定義はその定義だけ skip し、非カタログ登録に阻まれて
    // いる場合は次の refresh で再試行させる。
    let mut routine_blocked = false;
    for (key, def) in routines {
        let lower = key.to_ascii_lowercase();
        if crate::sql::udf_call::is_reserved_function_name(&key) {
            continue;
        }
        if registry.is_name_taken(&lower) {
            routine_blocked = true;
            continue;
        }
        if registry.len() >= MAX_SESSION_UDFS {
            routine_blocked |= registry.len() > registry.catalog_entry_count();
            continue;
        }
        registry.insert_routine(&key, def);
        registry.mark_catalog_origin(&key);
    }
    // 表せる形なのに登録できなかった定義（セッションローカル／WASM 関数との名前衝突・上限など）が
    // 残る場合は世代を「未完了」として記録し、次の refresh で再試行させる（ローカル登録の解除後に
    // カタログ定義が見えない状態を残さない）。
    // 恒久的にロード不能な定義（呼び出し先の消失・予約名など）は世代が変わらない限り結果も変わらないため
    // 再試行の対象にしない（残留すると Query／Parse ごとに全カタログを再ロードして世代 fast path を迂回する）。
    // 再試行が意味を持つのは、非カタログ由来の登録（セッションローカル宣言的関数・WASM 関数）に阻まれている
    // 定義だけ: (1) 同名の非カタログ登録との衝突（WASM 含む）、(2) 非カタログ登録が枠を占めて
    // MAX_SESSION_UDFS に達している場合。いずれもその登録の DROP で解消しうる。
    let has_non_catalog_entries = registry.len() > registry.catalog_entry_count();
    let at_capacity = registry.len() >= MAX_SESSION_UDFS;
    let blocked_by_local = routine_blocked
        || pending.iter().any(|p| {
            let lower = p.key.to_ascii_lowercase();
            (registry.is_name_taken(&lower) && !registry.is_catalog_origin(&lower))
            // 容量だけが阻害要因の定義（容量を無視すれば登録できる）に限る。欠落 callee・予約名など
            // 恒久的にロード不能な定義は容量が空いても登録できないため再試行対象にしない。
            || (at_capacity
                && has_non_catalog_entries
                && prepare_function_with_signature_ignoring_capacity(
                    registry,
                    &p.key,
                    &p.params,
                    &p.param_types,
                    p.return_type.as_ref(),
                    &p.body,
                )
                .is_ok())
        });
    let recorded = if blocked_by_local {
        STALE_GENERATION
    } else {
        generation
    };
    registry.set_catalog_binding(Some(tenant_id), recorded);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::function::{
        FunctionAttributes, FunctionParam, FunctionType, FUNCTIONS_TABLE,
    };
    use crate::catalog::ColumnType;
    use crate::sql::udf_call::prepare_function_definition;

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn decl(key: &str, params: &[&str], body: &str) -> FunctionDef {
        FunctionDef {
            id: 1,
            function_key: key.to_string(),
            params: params
                .iter()
                .map(|p| FunctionParam {
                    name: Some((*p).to_string()),
                    ty: None,
                })
                .collect(),
            return_type: None,
            return_shape: crate::catalog::function::ReturnShape::Scalar,
            language: FunctionLanguage::Declarative,
            body_source: body.to_string(),
            attrs: FunctionAttributes::default(),
        }
    }

    /// SQL-66・#1921: 壊れた blob があるとテナント全体のロードは `Err`（`XX000`・断片を含まない）。
    #[test]
    fn corrupt_blob_fails_the_whole_load() {
        let (s, _g) = tmp_storage("udf-load-corrupt");
        s.create_function("t", &decl("ok", &["x"], "x + 1"))
            .expect("create");
        let w = s.begin_write_txn().expect("w");
        {
            let mut table = w.open_table(FUNCTIONS_TABLE).expect("table");
            table
                .insert(("t", "broken", ""), b"SECRETFRAGMENT".as_slice())
                .expect("insert");
        }
        crate::recovery::commit_boundary::commit(w).expect("commit");
        let mut reg = UdfRegistry::default();
        let err = load(&s, "t", &mut reg).expect_err("must fail");
        assert_eq!(err.wire_code(), "XX000");
        assert!(!err.client_message().contains("SECRET"));
        assert!(!err.to_string().contains("SECRET"));
        assert!(reg.is_empty());
    }

    /// 予約名と衝突する定義・レジストリで表せない形の定義は skip され、他の定義はロードされる。
    #[test]
    fn unloadable_definitions_are_skipped_individually() {
        let (s, _g) = tmp_storage("udf-load-skip");
        s.create_function("t", &decl("good", &["x"], "x * 2"))
            .expect("good");
        s.create_function("t", &decl("concat", &["x"], "x"))
            .expect("reserved");
        // 正規形でない型（numeric(10,2)）の行は定義単位で skip される（#2798）。
        let mut typed = decl("typed", &["x"], "x");
        typed.params[0].ty = Some(
            FunctionType::from_column_type(&ColumnType::Numeric {
                precision: 10,
                scale: 2,
            })
            .expect("ty"),
        );
        s.create_function("t", &typed).expect("typed");
        // 型注釈の混在・型なし引数つきの RETURNS も skip される。
        let mut mixed = decl("mixed", &["x", "y"], "x");
        mixed.params[0].ty =
            Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty"));
        s.create_function("t", &mixed).expect("mixed");
        let mut ret_untyped = decl("retu", &["x"], "x");
        ret_untyped.return_type =
            Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty"));
        s.create_function("t", &ret_untyped).expect("retu");
        let mut sql_lang = decl("sqllang", &["x"], "x");
        sql_lang.language = FunctionLanguage::Sql;
        s.create_function("t", &sql_lang).expect("lang");
        s.create_function("t", &decl("badbody", &["x"], "x +"))
            .expect("badbody");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        assert!(reg.get("good").is_some());
        assert!(reg.is_catalog_origin("good"));
        assert_eq!(reg.len(), 1, "only good is loaded");
        assert_eq!(reg.catalog_scope(), Some("t"));
    }

    /// 呼び出し先の id の方が大きくても多パスで依存が解決される。消えた呼び出し先を持つ定義は skip。
    #[test]
    fn dependency_order_is_resolved_across_passes() {
        let (s, _g) = tmp_storage("udf-load-order");
        s.create_function("t", &decl("a", &["x"], "b ( x ) + 1"))
            .expect("a");
        s.create_function("t", &decl("b", &["x"], "x * 3"))
            .expect("b");
        s.create_function("t", &decl("orphan", &["x"], "gone ( x )"))
            .expect("orphan");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        assert!(reg.get("a").is_some() && reg.get("b").is_some());
        assert!(reg.get("orphan").is_none());
        // 恒久的にロード不能な定義が残っても世代は完了扱いで記録され、refresh が再ロードし続けない。
        assert_eq!(reg.catalog_generation(), s.function_generation("t"));
    }

    /// 他テナントの定義はロードされず、束縛と異なるテナントでの load／refresh はカタログ由来を消す。
    #[test]
    fn tenant_binding_is_enforced() {
        let (s, _g) = tmp_storage("udf-load-tenant");
        s.create_function("t", &decl("f", &["x"], "x")).expect("f");
        let mut reg = UdfRegistry::default();
        load(&s, "other", &mut reg).expect("load other");
        assert!(reg.is_empty());
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load t");
        assert!(reg.get("f").is_some());
        refresh(&s, "other", &mut reg);
        assert!(reg.get("f").is_none());
        assert!(load(&s, "other", &mut reg).is_err());
    }

    /// 登録できず skip した定義がある間は世代が未完了扱いになり、衝突が解消されたら refresh で見える。
    #[test]
    fn skipped_definition_is_retried_by_refresh() {
        let (s, _g) = tmp_storage("udf-load-retry");
        s.create_function("t", &decl("f", &["x"], "x + 1"))
            .expect("f");
        let mut reg = UdfRegistry::default();
        // 同名のセッションローカル関数（カタログ由来ではない）が先に居る。
        let body = {
            let toks = crate::sql::lexer::tokenize_rendered("x").expect("tok");
            parse_stored_function_body(&toks).expect("body")
        };
        let prepared =
            prepare_function_definition(&reg, "f", &["x".to_string()], &body).expect("prep");
        commit_function_definition(&mut reg, prepared);
        load(&s, "t", &mut reg).expect("load");
        assert!(!reg.is_catalog_origin("f"));
        // ローカル登録が消えた後、世代が変わらなくても refresh が再試行して見えるようになる。
        assert!(reg.remove("f"));
        refresh(&s, "t", &mut reg);
        assert!(reg.is_catalog_origin("f"));
    }

    /// 上限到達で登録できなかった定義は、ローカル関数の DROP 後の refresh で見える。
    #[test]
    fn capacity_blocked_definition_is_retried_after_local_drop() {
        let (s, _g) = tmp_storage("udf-load-cap");
        let mut reg = UdfRegistry::default();
        let body = {
            let toks = crate::sql::lexer::tokenize_rendered("x").expect("tok");
            parse_stored_function_body(&toks).expect("body")
        };
        for i in 0..MAX_SESSION_UDFS {
            let name = format!("loc{i}");
            let prepared =
                prepare_function_definition(&reg, &name, &["x".to_string()], &body).expect("prep");
            commit_function_definition(&mut reg, prepared);
        }
        s.create_function("t", &decl("f", &["x"], "x + 1"))
            .expect("f");
        load(&s, "t", &mut reg).expect("load");
        assert!(reg.get("f").is_none());
        assert!(reg.remove("loc0"));
        refresh(&s, "t", &mut reg);
        assert!(reg.is_catalog_origin("f"));
    }

    /// 容量満杯でも、恒久的にロード不能な定義（呼び出し先の消失）だけなら世代は完了扱いで記録する。
    #[test]
    fn permanent_skip_at_capacity_does_not_mark_stale() {
        let (s, _g) = tmp_storage("udf-load-cap-perm");
        let mut reg = UdfRegistry::default();
        let body = {
            let toks = crate::sql::lexer::tokenize_rendered("x").expect("tok");
            parse_stored_function_body(&toks).expect("body")
        };
        for i in 0..MAX_SESSION_UDFS {
            let name = format!("loc{i}");
            let prepared =
                prepare_function_definition(&reg, &name, &["x".to_string()], &body).expect("prep");
            commit_function_definition(&mut reg, prepared);
        }
        s.create_function("t", &decl("orphan", &["x"], "gone ( x )"))
            .expect("orphan");
        load(&s, "t", &mut reg).expect("load");
        assert!(reg.get("orphan").is_none());
        assert_eq!(reg.catalog_generation(), s.function_generation("t"));
    }

    /// 世代が進んだら refresh が再ロードし、カタログに無くなった定義は消える。
    #[test]
    fn refresh_follows_generation() {
        let (s, _g) = tmp_storage("udf-load-refresh");
        s.create_function("t", &decl("f", &["x"], "x")).expect("f");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        assert!(s
            .drop_function_by_name("t", "f", false)
            .expect("drop")
            .is_some());
        refresh(&s, "t", &mut reg);
        assert!(reg.get("f").is_none());
        s.create_function("t", &decl("g", &["x"], "x")).expect("g");
        refresh(&s, "t", &mut reg);
        assert!(reg.get("g").is_some());
    }

    /// SQL-66・#2798: 正規形の型付き行（RETURNS つき）はロードされ、型が復元される。
    #[test]
    fn canonical_typed_definition_is_loaded_with_types() {
        let (s, _g) = tmp_storage("udf-load-typed");
        let mut f = decl("inc", &["x"], "x + 1");
        f.params[0].ty = Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty"));
        f.return_type = Some(FunctionType::from_column_type(&ColumnType::BigInt).expect("ty"));
        s.create_function("t", &f).expect("inc");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        let d = reg.get("inc").expect("loaded");
        assert_eq!(d.param_types, vec![Some(CastType::Int4)]);
        assert_eq!(d.return_type, Some(CastType::Int8));
    }

    /// SQL-66・#2798: 本変更前に書かれた v1 の型なし blob は従来どおりロードでき、版が未知の blob は
    /// テナント全体のロードを `XX000` で拒否する（版は上げていない）。
    #[test]
    fn legacy_untyped_v1_blob_loads_and_unknown_version_fails_closed() {
        let (s, _g) = tmp_storage("udf-load-legacy");
        s.create_function("t", &decl("old", &["x"], "x + 1"))
            .expect("old");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        assert!(reg.get("old").is_some());
        let w = s.begin_write_txn().expect("w");
        {
            let mut table = w.open_table(FUNCTIONS_TABLE).expect("table");
            table
                .insert(("t", "future", ""), [2u8, 1, 2, 3].as_slice())
                .expect("insert");
        }
        crate::recovery::commit_boundary::commit(w).expect("commit");
        let err = load(&s, "t", &mut reg).expect_err("unknown version");
        assert_eq!(err.wire_code(), "XX000");
    }
}
