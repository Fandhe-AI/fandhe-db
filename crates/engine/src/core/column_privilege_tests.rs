//! 列単位の権限検査（SQL-40 ポインタ。Issue #2083。ADR `docs/design/column-level-privilege.md` の C1〜C5）の
//! 結合テスト。
//!
//! 現在ロールを束縛したセッションで、`GRANT ... (列)` の列単位の付与が読み取り・`INSERT`・`UPDATE`・
//! `DELETE`・`COPY` の各ゲートで「文が名指しする列」に効くこと、判別できない形（`*`・JOIN 等）は全列、
//! 従来方式の表の疑似列 `id` とビューは表単位でしか満たせないこと、`42501` の文言が列名・表名を含まない
//! ことを固定する。全経路の越境マトリクスは別 Issue（#2084）が担う。
//! ロールを作る `Storage::create_role` が `pub(crate)` のため crate 内に置く。

use super::EngineCore;
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, RowIdentity, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const A: &str = "tenant-a";

struct Fx {
    core: EngineCore,
    _g: CleanupGuard,
    admin: u64,
    alice: u64,
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(A, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn role(storage: &Storage, name: &str) -> u64 {
    storage
        .create_role(
            A,
            &RoleDef {
                id: 0,
                name: name.to_string(),
                attributes: RoleAttributes::default(),
                connection_limit: -1,
                valid_until: None,
                scram_verifier: Vec::new(),
                legacy_phc: Vec::new(),
            },
        )
        .expect("role")
}

/// `ct` は従来方式の表（疑似列 `id` あり）、`ca2` は採番方式の表（`id` は疑似列でない）。
fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            "ct",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::BigInt, false),
                ColumnDef::new("m", ColumnType::BigInt, false),
            ],
        ))
        .expect("ct");
    storage
        .create_table(
            &TableSchema::new(
                "ca2",
                vec![
                    ColumnDef::new("a", ColumnType::BigInt, true),
                    ColumnDef::new("b", ColumnType::BigInt, true),
                    ColumnDef::new("c", ColumnType::BigInt, true),
                ],
            )
            .with_row_identity(RowIdentity::Allocated),
        )
        .expect("ca2");
    // 従来方式の表で実列 `id` を宣言したもの（実列が名前 `id` を遮蔽するが、`*` は疑似列 `id` も返す）。
    storage
        .create_table(&TableSchema::new(
            "cr",
            vec![
                ColumnDef::new("id", ColumnType::BigInt, false),
                ColumnDef::new("n", ColumnType::BigInt, false),
            ],
        ))
        .expect("cr");
    let admin = role(&storage, "admin_r");
    let alice = role(&storage, "alice");
    Fx {
        core: EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        _g: g,
        admin,
        alice,
    }
}

fn bound(id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(A));
    if ddl {
        s.allow_ddl();
    }
    s
}

fn code(r: Result<crate::sql::SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

impl Fx {
    fn admin_run(&self, sql: &str) {
        let mut s = bound(self.admin, true);
        self.core
            .execute_sql_in_session(&ctx(), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }

    fn alice(&self, sql: &str) -> String {
        let mut s = bound(self.alice, false);
        code(self.core.execute_sql_in_session(&ctx(), &mut s, sql))
    }

    fn alice_err(&self, sql: &str) -> SqlSurfaceError {
        let mut s = bound(self.alice, false);
        self.core
            .execute_sql_in_session(&ctx(), &mut s, sql)
            .expect_err("must be rejected")
    }

    fn copy(&self, sql: &str) -> String {
        let s = bound(self.alice, false);
        match self.core.begin_copy(&ctx(), &s, sql) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.wire_code().to_string(),
        }
    }
}

#[test]
fn column_grant_satisfies_reads_that_name_only_granted_columns() {
    let f = fx("colpriv-read");
    assert_eq!(f.alice("SELECT n FROM ct LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT (n) ON ct TO alice");
    assert_eq!(f.alice("SELECT n FROM ct LIMIT 5"), "ok");
    assert_eq!(f.alice("SELECT n FROM ct WHERE n = 1 LIMIT 5"), "ok");
    assert_eq!(f.alice("SELECT n FROM ct ORDER BY n LIMIT 5"), "ok");
    // 許可外の列は、投影・WHERE・ORDER BY のどこに現れても拒否する。
    assert_eq!(f.alice("SELECT m FROM ct LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT n FROM ct WHERE m = 1 LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT n FROM ct ORDER BY m LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT n, m FROM ct LIMIT 5"), "42501");
    // 距離順の対象ベクトル列も列として要求する。
    assert_eq!(
        f.alice("SELECT n FROM ct ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3"),
        "42501"
    );
    f.admin_run("GRANT SELECT (embedding) ON ct TO alice");
    assert_eq!(
        f.alice("SELECT n FROM ct ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3"),
        "ok"
    );
}

#[test]
fn star_and_legacy_id_need_table_level_privilege() {
    let f = fx("colpriv-star");
    f.admin_run("GRANT SELECT (embedding) ON ct TO alice");
    f.admin_run("GRANT SELECT (n) ON ct TO alice");
    f.admin_run("GRANT SELECT (m) ON ct TO alice");
    // 全実カラムに列単位の付与があっても、従来方式の表の疑似列 `id` は表単位でしか満たせない。
    assert_eq!(f.alice("SELECT * FROM ct LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT id FROM ct LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT n FROM ct LIMIT 5"), "ok");
    // 採番方式の表（`id` が疑似列でない）は、全列への列単位の付与で `*` も読める。
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (b) ON ca2 TO alice");
    assert_eq!(f.alice("SELECT * FROM ca2 LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT (c) ON ca2 TO alice");
    assert_eq!(f.alice("SELECT * FROM ca2 LIMIT 5"), "ok");
    assert_eq!(f.alice("SELECT a FROM ca2 LIMIT 5"), "ok");
}

#[test]
fn columnless_reads_need_any_column_grant() {
    let f = fx("colpriv-any");
    assert_eq!(f.alice("SELECT count(*) FROM ca2"), "42501");
    f.admin_run("GRANT SELECT (b) ON ca2 TO alice");
    assert_eq!(f.alice("SELECT count(*) FROM ca2"), "ok");
}

#[test]
fn joins_and_subqueries_demand_every_column() {
    let f = fx("colpriv-join");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (n) ON ct TO alice");
    assert_eq!(
        f.alice("SELECT ca2.a FROM ca2 JOIN ct ON ca2.a = ct.n LIMIT 5"),
        "42501"
    );
    f.admin_run("GRANT SELECT (b) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (c) ON ca2 TO alice");
    f.admin_run("GRANT SELECT ON ct TO alice");
    assert_eq!(
        f.alice("SELECT ca2.a FROM ca2 JOIN ct ON ca2.a = ct.n LIMIT 5"),
        "ok"
    );
}

#[test]
fn denial_is_fixed_text_and_error_order_hides_columns() {
    let f = fx("colpriv-order");
    // 付与が何も無いロールは、存在しない列でも `42501`（列の解決より前）。
    assert_eq!(f.alice("SELECT nosuch FROM ca2 LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    // 付与を持つロールは、存在しない列を従来どおり束縛段が `42703` にする。
    assert_eq!(f.alice("SELECT nosuch FROM ca2 LIMIT 5"), "42703");
    let e = f.alice_err("SELECT b FROM ca2 LIMIT 5");
    assert_eq!(e.wire_code(), "42501");
    let text = format!("{e} {e:?} {}", e.client_message());
    for needle in ["ca2", "\"b\"", "a, b", "column", "alice"] {
        assert!(!text.contains(needle), "{needle:?} leaked in {text}");
    }
    let star = f.alice_err("SELECT * FROM ca2 LIMIT 5");
    assert_eq!(star.client_message(), e.client_message());
    // 解決より前の段のエラーは従来どおり。
    assert_eq!(f.alice("SELECT FROM FROM ca2"), "42601");
    assert_eq!(f.alice("SELECT a FROM nosuch LIMIT 5"), "42P01");
}

#[test]
fn revoking_the_column_denies_again() {
    let f = fx("colpriv-revoke");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    assert_eq!(f.alice("SELECT a FROM ca2 LIMIT 5"), "ok");
    f.admin_run("REVOKE SELECT (a) ON ca2 FROM alice");
    assert_eq!(f.alice("SELECT a FROM ca2 LIMIT 5"), "42501");
}

#[test]
fn insert_needs_privilege_on_the_listed_columns() {
    let f = fx("colpriv-insert");
    f.admin_run("GRANT INSERT (a) ON ca2 TO alice");
    f.admin_run("GRANT INSERT (b) ON ca2 TO alice");
    assert_eq!(f.alice("INSERT INTO ca2 (a, b) VALUES (1, 2)"), "ok");
    assert_eq!(f.alice("INSERT INTO ca2 (a, c) VALUES (1, 2)"), "42501");
    // 列リストの省略は全列への INSERT。
    assert_eq!(f.alice("INSERT INTO ca2 VALUES (1, 2, 3)"), "42501");
    // RETURNING が読む列は SELECT 権限で判定する。
    assert_eq!(
        f.alice("INSERT INTO ca2 (a, b) VALUES (1, 2) RETURNING a"),
        "42501"
    );
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    assert_eq!(
        f.alice("INSERT INTO ca2 (a, b) VALUES (1, 2) RETURNING a"),
        "ok"
    );
    assert_eq!(
        f.alice("INSERT INTO ca2 (a, b) VALUES (1, 2) RETURNING c"),
        "42501"
    );
}

#[test]
fn on_conflict_on_constraint_needs_select_on_the_constraint_columns() {
    let f = fx("colpriv-conflict-constraint");
    f.admin_run("ALTER TABLE ca2 ADD CONSTRAINT uq_c UNIQUE (c)");
    f.admin_run("GRANT INSERT (c) ON ca2 TO alice");
    f.admin_run("GRANT INSERT (a) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    // 制約の対象列 c への SELECT なしでは、制約名経由の衝突判定を通さない（fail-closed）。
    assert_eq!(
        f.alice(
            "INSERT INTO ca2 (c, a) VALUES (1, 2) ON CONFLICT ON CONSTRAINT uq_c DO NOTHING RETURNING a"
        ),
        "42501"
    );
    f.admin_run("GRANT SELECT (c) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (b) ON ca2 TO alice");
    assert_eq!(
        f.alice(
            "INSERT INTO ca2 (c, a) VALUES (1, 2) ON CONFLICT ON CONSTRAINT uq_c DO NOTHING RETURNING a"
        ),
        "ok"
    );
}

#[test]
fn update_and_delete_split_write_columns_from_read_columns() {
    let f = fx("colpriv-update");
    f.admin_run("GRANT UPDATE (b) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    assert_eq!(f.alice("UPDATE ca2 SET b = 1 WHERE a = 2"), "ok");
    assert_eq!(f.alice("UPDATE ca2 SET c = 1 WHERE a = 2"), "42501");
    assert_eq!(f.alice("UPDATE ca2 SET b = 1 WHERE c = 2"), "42501");
    // 右辺で読む列は SELECT が要る（SET 対象の b は UPDATE だけでは読めない）。
    assert_eq!(f.alice("UPDATE ca2 SET b = b + 1 WHERE a = 2"), "42501");
    // DELETE は列単位の付与では満たせない。
    assert_eq!(f.alice("DELETE FROM ca2 WHERE a = 2"), "42501");
    f.admin_run("GRANT DELETE ON ca2 TO alice");
    assert_eq!(f.alice("DELETE FROM ca2 WHERE a = 2"), "ok");
    assert_eq!(f.alice("DELETE FROM ca2 WHERE c = 2"), "42501");
}

#[test]
fn copy_uses_the_column_list() {
    let f = fx("colpriv-copy");
    f.admin_run("GRANT SELECT (a) ON ca2 TO alice");
    f.admin_run("GRANT SELECT (b) ON ca2 TO alice");
    assert_eq!(f.copy("COPY ca2 (a, b) TO STDOUT"), "ok");
    assert_eq!(f.copy("COPY ca2 (a, c) TO STDOUT"), "42501");
    assert_eq!(f.copy("COPY ca2 TO STDOUT"), "42501");
}

#[test]
fn unbound_and_owner_equivalent_ignore_column_grants() {
    let f = fx("colpriv-bypass");
    let mut unbound = SessionState::default();
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(), &mut unbound, "SELECT * FROM ca2 LIMIT 5")
        ),
        "ok"
    );
    let mut admin = bound(f.admin, true);
    assert_eq!(
        code(
            f.core
                .execute_sql_in_session(&ctx(), &mut admin, "SELECT * FROM ca2 LIMIT 5")
        ),
        "ok"
    );
}

#[test]
fn star_on_legacy_table_with_real_id_column_still_needs_table_level_select() {
    let f = fx("colpriv-realid");
    f.admin_run("GRANT SELECT (id) ON cr TO alice");
    f.admin_run("GRANT SELECT (n) ON cr TO alice");
    // 実列 `id` が名前を遮蔽しても、`*` の展開は疑似列 `id`（行キー）を加えるので全実列の付与では足りない。
    assert_eq!(f.alice("SELECT * FROM cr LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT cr.* FROM cr LIMIT 5"), "42501");
    assert_eq!(f.alice("SELECT count(*), id FROM cr GROUP BY id"), "ok");
    // 名前で指す `id` は実列に束縛されるので列単位の付与で読める。
    assert_eq!(f.alice("SELECT id, n FROM cr LIMIT 5"), "ok");
    f.admin_run("GRANT SELECT ON cr TO alice");
    assert_eq!(f.alice("SELECT * FROM cr LIMIT 5"), "ok");
}

#[test]
fn star_on_legacy_table_without_real_id_column_needs_table_level_select() {
    let f = fx("colpriv-noid");
    for c in ["embedding", "n", "m"] {
        f.admin_run(&format!("GRANT SELECT ({c}) ON ct TO alice"));
    }
    assert_eq!(f.alice("SELECT * FROM ct LIMIT 5"), "42501");
    f.admin_run("GRANT SELECT ON ct TO alice");
    assert_eq!(f.alice("SELECT * FROM ct LIMIT 5"), "ok");
}

#[test]
fn returning_star_on_legacy_table_needs_table_level_select() {
    let f = fx("colpriv-returning");
    f.admin_run("GRANT DELETE ON cr TO alice");
    f.admin_run("GRANT SELECT (id) ON cr TO alice");
    f.admin_run("GRANT SELECT (n) ON cr TO alice");
    assert_eq!(f.alice("DELETE FROM cr WHERE n = 1 RETURNING *"), "42501");
    f.admin_run("GRANT SELECT ON cr TO alice");
    assert_eq!(f.alice("DELETE FROM cr WHERE n = 1 RETURNING *"), "ok");
}

/// COPY FROM STDIN を開始し、1 行を流して `finish` までを返す。拒否は wire_code。
fn begin_copy_from(f: &Fx, sql: &str, row: &[u8]) -> Result<crate::sql::copy::CopyInBatch, String> {
    let s = bound(f.alice, false);
    match f.core.begin_copy(&ctx(), &s, sql) {
        Ok(super::CopyPlan::From(mut cs)) => {
            cs.feed(row).expect("feed");
            Ok(cs.finish().expect("finish"))
        }
        Ok(super::CopyPlan::To(..)) => panic!("expected COPY FROM plan"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

#[test]
fn copy_from_commit_recheck_uses_the_same_column_list_as_begin() {
    let f = fx("colpriv-copy-from");
    f.admin_run("GRANT INSERT (a) ON ca2 TO alice");
    f.admin_run("GRANT INSERT (b) ON ca2 TO alice");
    // 列リスト外の列を含む形は開始時に拒否する。
    assert_eq!(
        begin_copy_from(&f, "COPY ca2 (a, b, c) FROM STDIN", b"1\t2\t3\n").err(),
        Some("42501".to_string())
    );
    // 付与のある列だけを列リストに書けば、開始から commit まで通る。
    let batch = begin_copy_from(&f, "COPY ca2 (a, b) FROM STDIN", b"1\t2\n").expect("begin");
    f.core.commit_copy_in(&ctx(), batch).expect("commit");
    // begin 後に列リストの列を剥奪すれば、commit 時の再検査が同じ列集合で拒否する。
    let batch = begin_copy_from(&f, "COPY ca2 (a, b) FROM STDIN", b"3\t4\n").expect("begin");
    f.admin_run("REVOKE INSERT (b) ON ca2 FROM alice");
    let e = f.core.commit_copy_in(&ctx(), batch).expect_err("revoked");
    assert_eq!(e.wire_code(), "42501");
    // 列リストに無い列（c）の剥奪・付与は判定に影響しない。
    f.admin_run("GRANT INSERT (b) ON ca2 TO alice");
    let batch = begin_copy_from(&f, "COPY ca2 (a, b) FROM STDIN", b"5\t6\n").expect("begin");
    f.admin_run("GRANT INSERT (c) ON ca2 TO alice");
    f.admin_run("REVOKE INSERT (c) ON ca2 FROM alice");
    f.core.commit_copy_in(&ctx(), batch).expect("commit");
}
