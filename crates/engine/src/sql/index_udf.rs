//! 式索引・部分索引に現れた組み込み以外の関数の揮発性による判定（INDEX-10・INDEX-11、SQL-67 (3)、
//! Issue #2217）。
//!
//! 役割: 構文段（`sql::allowlist::validate_create_index_tokens`）はカタログを引けないため、組み込み以外の
//! 関数に達した時点で判定を保留する（`sql::index_expr::IndexUserFunctionRef`）。`core.rs` の
//! `CreateIndex` 腕が `require_ddl_permission` の後（権限の無い主体にはカタログを引かず `42501`）に
//! 本モジュールを呼び、**作成セッションのテナント scope だけ**の UDF カタログで宣言された揮発性から
//! エラーを決める。他テナントの関数の存在で結果は変わらない（存在オラクルにならない）。
//!
//! 必ずエラーを返す（索引を作る経路を持たない）。IMMUTABLE の `LANGUAGE sql|plpgsql` 関数も受け入れ
//! ない: 本体の実行は未実装で、索引の維持（空の UDF レジストリでの再束縛）で評価できず fail-open に
//! なるため。宣言的 UDF は内容で判定する従来方針（INDEX-10 の改訂注記）で `0A000` のまま。

use crate::catalog::function::{FunctionLanguage, Volatility};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::index_expr::{not_immutable_at, udf_unsupported, IndexUserFunctionRef};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// 保留された組み込み以外の関数の呼び出しから、`CREATE INDEX` が返すエラーを決める。判定順は
/// 1. セッションの WASM UDF → `0A000`
/// 2. 自テナントの同名・同引数数の定義が無い → `0A000`（PostgreSQL は `42883`。既知の差）
/// 3. 宣言的 UDF が含まれる → `0A000`
/// 4. 非 IMMUTABLE の `sql`／`plpgsql` 関数が含まれる → `42P17`
/// 5. それ以外（IMMUTABLE だが実行できない）→ `0A000`
///
/// カタログの読み取りエラーは成功扱いにせず `0A000` に倒す（fail-closed。詳細は応答に出さない）。
pub(crate) fn index_user_function_verdict(
    storage: &Storage,
    tenant_id: &str,
    session: &SessionState,
    r: &IndexUserFunctionRef,
) -> SqlSurfaceError {
    let unsupported = udf_unsupported(r.site);
    if session.udfs().get_wasm(&r.name).is_some() {
        return unsupported;
    }
    let Ok(overloads) = storage.list_overloads(tenant_id, &r.name) else {
        return unsupported;
    };
    let candidates: Vec<_> = overloads
        .iter()
        .filter(|d| d.params.len() == r.arity)
        .collect();
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|d| d.language == FunctionLanguage::Declarative)
    {
        return unsupported;
    }
    if candidates
        .iter()
        .any(|d| d.attrs.volatility != Volatility::Immutable)
    {
        return not_immutable_at(r.site);
    }
    unsupported
}
