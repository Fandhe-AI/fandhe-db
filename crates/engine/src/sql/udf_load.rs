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

use crate::catalog::function::{FunctionDef, FunctionLanguage, MAX_FUNCTIONS};
use crate::sql::allowlist::{parse_stored_function_body, SqlSurfaceError};
use crate::sql::udf_call::{commit_function_definition, prepare_function_definition};
use crate::sql::udf_call::{Expr, UdfRegistry};
use crate::storage::Storage;

/// refresh に失敗したセッションが次回必ず再試行するための世代（実在の世代と一致しない値）。
const STALE_GENERATION: u64 = u64::MAX;

/// ロード失敗の固定エラー（tenant_id・内部キー・blob の断片を含めない）。
fn load_failed() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "could not load function catalog".to_string(),
    }
}

/// レジストリ（宣言的 UDF）で表せる形か。型注釈・戻り値型・言語付きの行は後続の機能（#2215 等）が
/// 書くため、宣言的 UDF として誤ってロードしない。
fn representable(def: &FunctionDef) -> bool {
    def.language == FunctionLanguage::Declarative
        && def.return_type.is_none()
        && def
            .params
            .iter()
            .all(|p| p.ty.is_none() && p.name.is_some())
}

/// 再パース済みの登録待ち定義。
struct Pending {
    key: String,
    params: Vec<String>,
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
    for def in defs {
        if !representable(&def) {
            continue;
        }
        let Ok(tokens) = crate::sql::lexer::tokenize_rendered(&def.body_source) else {
            continue;
        };
        let Ok(body) = parse_stored_function_body(&tokens) else {
            continue;
        };
        let Some(params) = def
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Option<Vec<String>>>()
        else {
            continue;
        };
        pending.push(Pending {
            key: def.function_key,
            params,
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
            match prepare_function_definition(registry, &p.key, &p.params, &p.body) {
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
    // 表せる形なのに登録できなかった定義（セッションローカル／WASM 関数との名前衝突・上限など）が
    // 残る場合は世代を「未完了」として記録し、次の refresh で再試行させる（ローカル登録の解除後に
    // カタログ定義が見えない状態を残さない）。
    // 恒久的にロード不能な定義（呼び出し先の消失・予約名など）は世代が変わらない限り結果も変わらないため
    // 再試行の対象にしない（残留すると Query／Parse ごとに全カタログを再ロードして世代 fast path を迂回する）。
    // 再試行が意味を持つのは、同名のセッションローカル登録に阻まれている定義だけ。
    let blocked_by_local = pending
        .iter()
        .any(|p| registry.get(&p.key).is_some() && !registry.is_catalog_origin(&p.key));
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
        let mut typed = decl("typed", &["x"], "x");
        typed.params[0].ty =
            Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty"));
        s.create_function("t", &typed).expect("typed");
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

    /// 世代が進んだら refresh が再ロードし、カタログに無くなった定義は消える。
    #[test]
    fn refresh_follows_generation() {
        let (s, _g) = tmp_storage("udf-load-refresh");
        s.create_function("t", &decl("f", &["x"], "x")).expect("f");
        let mut reg = UdfRegistry::default();
        load(&s, "t", &mut reg).expect("load");
        assert!(s.drop_function_by_name("t", "f").expect("drop").is_some());
        refresh(&s, "t", &mut reg);
        assert!(reg.get("f").is_none());
        s.create_function("t", &decl("g", &["x"], "x")).expect("g");
        refresh(&s, "t", &mut reg);
        assert!(reg.get("g").is_some());
    }
}
