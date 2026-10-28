//! `INSERT ... SELECT ... ON CONFLICT` の Describe 段と実行段の fail-closed 一致（SQL-51・SQL-49。Issue #2614）。
//!
//! 移行済み表で疑似列 `id` を省略した UPSERT は実行本体が `0A000` で拒否する。Describe 段
//! （`bind_insert_select`）も同じ判定を共有し、「Describe は成功するのに Execute は必ず拒否」の
//! 食い違いを作らないことを固定する。ポインタ: `docs/design/insert-select.md`。

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Visibility;

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

#[test]
fn describe_and_execute_both_reject_omitted_id_upsert_on_migrated_table() {
    let path = unique_db_path("sql51-oc-describe-omit-id");
    let _g = CleanupGuard(path.clone());
    {
        let core = EngineCore::open(&path).expect("open");
        let mut s = session();
        for ddl in [
            "CREATE TABLE items (n BIGINT)",
            "CREATE TABLE src (n BIGINT)",
        ] {
            core.execute_sql_in_session_legacy(&ctx("sys"), &mut s, ddl)
                .expect("create");
        }
        core.execute_sql_in_session_legacy(
            &ctx("a"),
            &mut s,
            "INSERT INTO src (id, n) VALUES (1, 7)",
        )
        .expect("seed");
    }
    // 開き直しで従来方式の表が移行済みになる。
    let core = EngineCore::open(&path).expect("reopen migrates");
    {
        let clause = "ON CONFLICT DO NOTHING";
        let sql =
            format!("INSERT INTO items (n) SELECT n FROM src ORDER BY n LIMIT 100 {clause} USING OPERATION_ID 'oc-d'");
        let parsed = core.parse_sql(&sql).expect("parse");
        let s = SessionState::default();
        let describe = core.describe_parsed_in_session(&s, &parsed);
        let mut es = SessionState::default();
        let exec = core.execute_sql_in_session(&ctx("a"), &mut es, &sql);
        let dcode = describe
            .expect_err("describe must reject")
            .wire_code()
            .to_string();
        let ecode = exec
            .expect_err("execute must reject")
            .wire_code()
            .to_string();
        assert_eq!(dcode, "0A000", "{clause}");
        assert_eq!(ecode, dcode, "{clause}");
    }
}
