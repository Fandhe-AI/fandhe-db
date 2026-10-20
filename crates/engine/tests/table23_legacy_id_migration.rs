//! 既存データディレクトリを開く時点の `id` 移行（Issue #2200。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-12・TABLE-16、`rls.md` RLS-9・RLS-10）の
//! 結合テスト。
//!
//! 検証の軸: 従来方式の表（NoSQL 表層・Rust API と同じ作成経路）を持つ DB を閉じて開き直した後も、
//! `WHERE id = n`・`ON CONFLICT (id)`・`SELECT *`・集約・FK 違反・`id` 重複・`operation_id` の再送・
//! テナント境界の結果が移行前と一致すること。移行はカタログだけを書き換えるため、結果の不変を
//! 開き直し前後の同一プローブの比較で固定する。

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Visibility;

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 結果（成功なら出力の Debug、失敗なら `wire_code`）を文字列で返す。
fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    let mut s = SessionState::default();
    s.allow_ddl();
    match core.execute_sql_in_session_legacy(ctx, &mut s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => format!("err:{}", e.wire_code()),
    }
}

fn must(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let r = outcome(core, ctx, sql);
    assert!(r.starts_with("ok:"), "{sql}: {r}");
}

/// 状態を変えない問い合わせとエラーになる書き込みの集合。移行前後で同一でなければならない。
fn probe(core: &EngineCore) -> Vec<String> {
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    let mut out = Vec::new();
    // 読み取り（両テナント）。
    for sql in [
        "SELECT * FROM parents ORDER BY id LIMIT 100",
        "SELECT id, name FROM parents WHERE id = 2 LIMIT 100",
        "SELECT * FROM kids ORDER BY id LIMIT 100",
        "SELECT * FROM keyed ORDER BY id LIMIT 100",
        "SELECT COUNT(id), SUM(id), MIN(id), MAX(id) FROM parents",
        // 他テナントの行は見えない。
        "SELECT id, name FROM parents ORDER BY id LIMIT 100",
    ] {
        out.push(format!("a|{sql}|{}", outcome(core, &a, sql)));
        out.push(format!("b|{sql}|{}", outcome(core, &b, sql)));
    }
    // id 重複（23505）・FK 違反（23503）・operation_id の再送（状態を変えないエラーのみ。tenant-a）。
    for sql in [
        "INSERT INTO parents (id, name) VALUES (1, 'dup') USING OPERATION_ID 'op-dup'",
        "INSERT INTO kids (id, parent_id, v) VALUES (50, 999, 'x') USING OPERATION_ID 'op-fk'",
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
        "INSERT INTO keyed (id, code, n) VALUES (9, 'k1', 1) USING OPERATION_ID 'op-key-dup'",
    ] {
        out.push(format!("a|{sql}|{}", outcome(core, &a, sql)));
    }
    out
}

/// 受け入れ基準 1: 開き直し（移行）後も既存クエリの結果が変わらない。移行後の書き込みも従来どおり。
#[test]
fn queries_are_unchanged_after_reopen_migrates_legacy_tables() {
    let path = unique_db_path("table23-legacy-migrate");
    let _g = CleanupGuard(path.clone());
    let before = {
        let core = EngineCore::open(&path).expect("open");
        let a = ctx("tenant-a");
        let b = ctx("tenant-b");
        must(&core, &a, "CREATE TABLE parents (name TEXT)");
        must(
            &core,
            &a,
            "CREATE TABLE kids (parent_id BIGINT REFERENCES parents, v TEXT)",
        );
        must(
            &core,
            &a,
            "CREATE TABLE keyed (code TEXT PRIMARY KEY, n BIGINT)",
        );
        must(
            &core,
            &a,
            "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
        );
        must(
            &core,
            &a,
            "INSERT INTO parents (id, name) VALUES (2, 'p2') USING OPERATION_ID 'op-p2'",
        );
        must(
            &core,
            &b,
            "INSERT INTO parents (id, name) VALUES (1, 'b1') USING OPERATION_ID 'op-b1'",
        );
        must(
            &core,
            &a,
            "INSERT INTO kids (id, parent_id, v) VALUES (10, 1, 'k') USING OPERATION_ID 'op-k'",
        );
        must(
            &core,
            &a,
            "INSERT INTO keyed (id, code, n) VALUES (5, 'k1', 1) USING OPERATION_ID 'op-key'",
        );
        probe(&core)
    };
    // 比較が空振りしない（SELECT が実際に成功し、行を返している）。
    assert!(before[0].starts_with("a|SELECT * FROM parents") && before[0].contains("ok:"));
    assert!(
        before[0].contains("p1") && before[0].contains("p2"),
        "{}",
        before[0]
    );
    let core = EngineCore::open(&path).expect("reopen migrates");
    assert_eq!(before, probe(&core));
    // 開き直しを繰り返しても変わらない（2 回目以降の移行は何もしない）。
    drop(core);
    let core = EngineCore::open(&path).expect("reopen again");
    assert_eq!(before, probe(&core));

    // 移行後の書き込みも従来どおり（ON CONFLICT (id)・id 指定の UPDATE／DELETE）。
    let a = ctx("tenant-a");
    must(
        &core,
        &a,
        "INSERT INTO parents (id, name) VALUES (2, 'p2x') ON CONFLICT (id) DO UPDATE \
         SET name = EXCLUDED.name USING OPERATION_ID 'op-up'",
    );
    must(
        &core,
        &a,
        "INSERT INTO parents (id, name) VALUES (3, 'p3') ON CONFLICT (id) DO NOTHING \
         USING OPERATION_ID 'op-3'",
    );
    must(
        &core,
        &a,
        "UPDATE parents SET name = 'p1x' WHERE id = 1 USING OPERATION_ID 'op-u1'",
    );
    let after = outcome(
        &core,
        &a,
        "SELECT id, name FROM parents ORDER BY id LIMIT 100",
    );
    assert!(
        after.contains("p1x") && after.contains("p2x") && after.contains("p3"),
        "{after}"
    );
    // テナント境界: 他テナントの行は更新されていない。
    let b = outcome(
        &core,
        &ctx("tenant-b"),
        "SELECT id, name FROM parents ORDER BY id LIMIT 100",
    );
    assert!(b.contains("b1") && !b.contains("p1x"), "{b}");
    must(
        &core,
        &a,
        "DELETE FROM parents WHERE id = 3 USING OPERATION_ID 'op-d3'",
    );
}
