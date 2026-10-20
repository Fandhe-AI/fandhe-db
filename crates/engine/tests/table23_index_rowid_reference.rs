//! 二次索引・FK・UNIQUE の行参照の rowid 化（TABLE-23・Issue #2194。設計記録:
//! `docs/design/rowid-primary-key-separation.md`）の結合テスト。
//!
//! 役割: 永続一意索引（`user_uniq`）・FK 用キー索引・FK 検査が行参照を rowid 型で扱うように
//! なっても、(1) 再オープン後に索引が無効化・再構築されずそのまま使えること（永続バイト列の
//! 不変）、(2) `23505`／`23503` の SQLSTATE が変わらないこと、(3) エラー文面に rowid（数値）が
//! 混入しないことを公開 API（SQL 表層）から固定する。rowid とキー値が食い違うケースと
//! 採番方式の親の拒否は crate 内部の単体テスト（`src/constraint.rs`）が持つ。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 成功を要求して実行する。
fn ok(core: &EngineCore, session: &mut SessionState, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session_legacy(ctx, session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

/// 失敗を要求し、`(wire_code, 表示文面)` を返す。
fn fail(
    core: &EngineCore,
    session: &mut SessionState,
    ctx: &PolicyContext,
    sql: &str,
) -> (String, String) {
    let e = core
        .execute_sql_in_session_legacy(ctx, session, sql)
        .expect_err(&format!("{sql} must fail"));
    (e.wire_code().to_string(), e.to_string())
}

/// 書き込み後に閉じて再オープンしても、PK・UNIQUE・FK（列参照・`id` 参照）の検査が従来どおり働き、
/// SQLSTATE が変わらず、文面に行 id（rowid）が現れない。
#[test]
fn constraints_survive_reopen_with_same_sqlstate_and_no_rowid_in_messages() {
    let path = unique_db_path("table23-index-rowid-reopen");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("tenant-a");
    {
        let core = open_core(&path);
        let mut s = SessionState::default();
        s.allow_ddl();
        ok(
            &core,
            &mut s,
            &owner,
            "CREATE TABLE parents (code TEXT NOT NULL PRIMARY KEY, mail TEXT UNIQUE)",
        );
        ok(&core, &mut s, &owner, "CREATE TABLE anchors (label TEXT)");
        ok(
            &core,
            &mut s,
            &owner,
            "CREATE TABLE kids (pcode TEXT REFERENCES parents(code), pid BIGINT REFERENCES anchors)",
        );
        ok(
            &core,
            &mut s,
            &owner,
            "INSERT INTO parents (id, code, mail) VALUES (7777, 'p1', 'a@x') USING OPERATION_ID 'op-p1'",
        );
        ok(
            &core,
            &mut s,
            &owner,
            "INSERT INTO anchors (id, label) VALUES (7777, 'x') USING OPERATION_ID 'op-an'",
        );
        ok(
            &core,
            &mut s,
            &owner,
            "INSERT INTO kids (id, pcode, pid) VALUES (9001, 'p1', 7777) USING OPERATION_ID 'op-k1'",
        );
    }
    // 再オープン: 索引は無効化されず、そのまま使われる。
    let core = open_core(&path);
    let mut s = SessionState::default();
    for (sql, code) in [
        (
            "INSERT INTO parents (id, code, mail) VALUES (7778, 'p1', 'b@x') USING OPERATION_ID 'op-d1'",
            "23505",
        ),
        (
            "INSERT INTO parents (id, code, mail) VALUES (7779, 'p2', 'a@x') USING OPERATION_ID 'op-d2'",
            "23505",
        ),
        (
            "INSERT INTO kids (id, pcode, pid) VALUES (9002, 'zzz', 7777) USING OPERATION_ID 'op-f1'",
            "23503",
        ),
        (
            "INSERT INTO kids (id, pcode, pid) VALUES (9003, 'p1', 424242) USING OPERATION_ID 'op-f2'",
            "23503",
        ),
        (
            "DELETE FROM parents WHERE code = 'p1' USING OPERATION_ID 'op-f3'",
            "23503",
        ),
        (
            "DELETE FROM anchors WHERE label = 'x' USING OPERATION_ID 'op-f4'",
            "23503",
        ),
    ] {
        let (wire, text) = fail(&core, &mut s, &owner, sql);
        assert_eq!(wire, code, "{sql}");
        for rowid_like in ["7777", "7778", "7779", "9001", "9002", "9003", "424242"] {
            assert!(!text.contains(rowid_like), "{sql}: {text}");
        }
    }
    // 子を消せば親も消せる（索引の逆引きが再オープン後も整合している）。
    ok(
        &core,
        &mut s,
        &owner,
        "DELETE FROM kids WHERE pcode = 'p1' USING OPERATION_ID 'op-ok1'",
    );
    ok(
        &core,
        &mut s,
        &owner,
        "DELETE FROM anchors WHERE label = 'x' USING OPERATION_ID 'op-ok4'",
    );
    ok(
        &core,
        &mut s,
        &owner,
        "DELETE FROM parents WHERE code = 'p1' USING OPERATION_ID 'op-ok2'",
    );
    // 親を消した後は同じキー値を別の行 id で再利用できる。
    ok(
        &core,
        &mut s,
        &owner,
        "INSERT INTO parents (id, code, mail) VALUES (8000, 'p1', 'a@x') USING OPERATION_ID 'op-ok3'",
    );
}

/// 他テナントの行は、一意性・FK の判定母集合に入らない（テナント境界は変わらない）。
#[test]
fn tenant_boundary_is_unchanged_for_unique_and_fk() {
    let path = unique_db_path("table23-index-rowid-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    let mut s = SessionState::default();
    s.allow_ddl();
    ok(
        &core,
        &mut s,
        &a,
        "CREATE TABLE parents (code TEXT NOT NULL PRIMARY KEY)",
    );
    ok(
        &core,
        &mut s,
        &a,
        "CREATE TABLE kids (pcode TEXT REFERENCES parents(code))",
    );
    ok(
        &core,
        &mut s,
        &a,
        "INSERT INTO parents (id, code) VALUES (1, 'p1') USING OPERATION_ID 'op-a1'",
    );
    // 同じ PK 値・同じ id でも別テナントなら衝突しない。
    ok(
        &core,
        &mut s,
        &b,
        "INSERT INTO parents (id, code) VALUES (1, 'p1') USING OPERATION_ID 'op-b1'",
    );
    // tenant-b からは tenant-a にだけある親を参照できない。
    ok(
        &core,
        &mut s,
        &a,
        "INSERT INTO parents (id, code) VALUES (2, 'only-a') USING OPERATION_ID 'op-a2'",
    );
    let (wire, _) = fail(
        &core,
        &mut s,
        &b,
        "INSERT INTO kids (id, pcode) VALUES (1, 'only-a') USING OPERATION_ID 'op-b2'",
    );
    assert_eq!(wire, "23503");
}
