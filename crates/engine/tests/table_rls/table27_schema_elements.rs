//! 要素つき `CREATE SCHEMA <名> CREATE TABLE ... CREATE VIEW ...`（TABLE-27・TABLE-44・SQL-39・SQL-40・
//! RLS-7・RLS-8・RLS-10 のポインタ、Issue #3286）の結合テスト。設計記録は
//! `docs/design/schema-tenant-namespace.md`（「Issue #3286 実装記録」の D1〜D9）。
//!
//! `table27_schema_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - SCHEMA と全要素（TABLE・VIEW・SEQUENCE）が 1 つの write txn で作られ、どれかが失敗すれば何も残らない
//! - 再起動（Storage の再オープン）後も残り、失敗後の再オープンでも残骸が無い
//! - 他テナントの同名 SCHEMA・オブジェクトに影響せず、存在も漏らさない
//! - 要素ごとに単独の DDL と同じ権限ゲート（未許可は常に `42501`）
//! - 未対応の形は `0A000`、構文違反は `42601`、別 SCHEMA での作成は `42P15`、要素数の上限は `54000`

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

struct Fx {
    path: std::path::PathBuf,
    _g: CleanupGuard,
    core: EngineCore,
}

fn core_over(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    Fx {
        path,
        _g: guard,
        core: core_over(storage),
    }
}

impl Fx {
    fn run(
        &self,
        tenant: &str,
        s: &mut SessionState,
        sql: &str,
    ) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
        self.core.execute_sql_in_session(&ctx(tenant), s, sql)
    }

    fn ok(&self, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
        self.run(tenant, s, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {} {e}", e.wire_code()))
    }

    fn err(&self, tenant: &str, s: &mut SessionState, sql: &str) -> (String, String) {
        match self.run(tenant, s, sql) {
            Ok(o) => panic!("{sql} must fail, got {o:?}"),
            Err(e) => (e.wire_code().to_string(), e.to_string()),
        }
    }

    fn code(&self, tenant: &str, s: &mut SessionState, sql: &str) -> String {
        self.err(tenant, s, sql).0
    }

    fn rows(&self, tenant: &str, s: &mut SessionState, sql: &str) -> usize {
        match self.ok(tenant, s, sql) {
            SqlOutcome::Query(r) => r.rows.len(),
            other => panic!("expected Query for {sql}, got {other:?}"),
        }
    }

    /// DB を閉じて開き直す（再起動後の永続性・残骸の無さの確認）。
    fn reopen(self) -> Fx {
        let Fx { path, _g, core } = self;
        drop(core);
        let core = core_over(Storage::open(&path).expect("reopen storage"));
        Fx { path, _g, core }
    }
}

const A: &str = "alice";
const B: &str = "bob";

#[test]
fn table_and_view_elements_are_created_with_trailing_semicolon() {
    let f = fx("sch-el-basic");
    let mut s = ddl_session();
    let out = f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY, a INT) \
         CREATE VIEW v AS SELECT id, a FROM t;",
    );
    assert!(matches!(out, SqlOutcome::CreateSchema(_)), "{out:?}");
    f.ok(A, &mut s, "INSERT INTO s.t (id, a) VALUES (1, 10), (2, 20)");
    assert_eq!(
        f.rows(A, &mut s, "SELECT id FROM s.v ORDER BY id LIMIT 10"),
        2
    );
    assert_eq!(
        f.rows(A, &mut s, "SELECT id FROM s.t ORDER BY id LIMIT 10"),
        2
    );
    // 要素は新しい SCHEMA に作られ、public には作られない（既定の path で未修飾の `t` は見えない）。
    assert_eq!(f.code(A, &mut s, "SELECT id FROM t LIMIT 1"), "42P01");
    assert_eq!(f.code(A, &mut s, "SELECT id FROM v LIMIT 1"), "42P01");
    // 再起動後も残る。
    let f = f.reopen();
    assert_eq!(
        f.rows(A, &mut s, "SELECT id FROM s.v ORDER BY id LIMIT 10"),
        2
    );
    assert_eq!(f.code(A, &mut s, "CREATE SCHEMA s"), "42P06");
}

#[test]
fn same_schema_qualification_is_accepted_and_other_schema_is_42p15() {
    let f = fx("sch-el-qual");
    let mut s = ddl_session();
    f.ok(A, &mut s, "CREATE SCHEMA other");
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE s.t (id BIGINT PRIMARY KEY) CREATE TABLE t2 (id BIGINT REFERENCES s.t (id)) \
         CREATE VIEW s.v AS SELECT id FROM s.t",
    );
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.v LIMIT 10"), 0);
    for (n, sql) in [
        "CREATE SCHEMA s1 CREATE TABLE public.x (a INT)",
        "CREATE SCHEMA s1 CREATE TABLE other.x (a INT)",
        "CREATE SCHEMA s1 CREATE TABLE t (a INT) CREATE VIEW other.v AS SELECT a FROM t",
        "CREATE SCHEMA s1 CREATE SEQUENCE other.q",
    ]
    .into_iter()
    .enumerate()
    {
        let (code, text) = f.err(A, &mut s, sql);
        assert_eq!(code, "42P15", "{sql}");
        assert!(!text.contains('#'), "internal key leaked: {text}");
        // 失敗した文は SCHEMA を残さない。
        assert_eq!(f.code(A, &mut s, "DROP SCHEMA s1"), "3F000", "case {n}");
    }
}

#[test]
fn elements_run_in_postgres_order_regardless_of_written_order() {
    let f = fx("sch-el-order");
    let mut s = ddl_session();
    // VIEW を TABLE より前に書いても、SEQUENCE → TABLE → VIEW の順に作られる。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE VIEW v AS SELECT id FROM t CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT DEFAULT nextval('q')) \
         CREATE SEQUENCE q START 5",
    );
    f.ok(A, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.v LIMIT 10"), 1);
}

#[test]
fn sequence_serial_and_foreign_keys_live_in_the_new_schema() {
    let f = fx("sch-el-seq");
    let mut s = ddl_session();
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE SEQUENCE q CREATE TABLE p (id SERIAL PRIMARY KEY, a INT) \
         CREATE TABLE c (id BIGINT PRIMARY KEY, pid INT REFERENCES p (id), k BIGINT DEFAULT nextval('s.q'))",
    );
    f.ok(A, &mut s, "INSERT INTO s.p (a) VALUES (1), (2)");
    f.ok(A, &mut s, "INSERT INTO s.c (id, pid) VALUES (1, 1)");
    assert_eq!(
        f.code(A, &mut s, "INSERT INTO s.c (id, pid) VALUES (2, 99)"),
        "23503"
    );
    // 暗黙 SEQUENCE も同じ SCHEMA にあり、削除は中身が残っている間 `2BP01`。
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "2BP01");
    f.ok(A, &mut s, "DROP SCHEMA s CASCADE");
    f.ok(A, &mut s, "CREATE SCHEMA s");
}

#[test]
fn dependency_on_public_is_rejected_like_a_single_ddl_and_leaves_nothing() {
    let f = fx("sch-el-public-dep");
    let mut s = ddl_session();
    f.ok(A, &mut s, "CREATE TABLE pubt (id BIGINT PRIMARY KEY)");
    for sql in [
        "CREATE SCHEMA s CREATE TABLE c (id BIGINT REFERENCES pubt (id))",
        "CREATE SCHEMA s CREATE VIEW v AS SELECT id FROM pubt",
        "CREATE SCHEMA s CREATE TABLE ok (id BIGINT PRIMARY KEY) CREATE VIEW v AS SELECT id FROM public.pubt",
    ] {
        assert_eq!(f.code(A, &mut s, sql), "42501", "{sql}");
    }
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "3F000");
}

#[test]
fn failure_of_any_element_leaves_neither_the_schema_nor_earlier_elements() {
    let f = fx("sch-el-atomic");
    let mut s = ddl_session();
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA keep CREATE TABLE k (id BIGINT PRIMARY KEY)",
    );
    let cases = [
        // 2 件目の VIEW が不在の表を参照する。
        ("CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE VIEW v AS SELECT id FROM s.nosuch", "42P01"),
        // 同じ文での重複名。
        ("CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE TABLE t (id BIGINT)", "42P07"),
        ("CREATE SCHEMA s CREATE SEQUENCE x CREATE TABLE x (id BIGINT)", "42P07"),
        // FK 先が無い・SEQUENCE の範囲違反・未知の型（ENUM 型の可能性があるため単独の DDL と同じ SCHEMA 越境依存の `42501`）。
        ("CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE TABLE c (id BIGINT REFERENCES s.nosuch (id))", "42P01"),
        ("CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE SEQUENCE q MINVALUE 5 MAXVALUE 1", "22023"),
        ("CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE TABLE u (a NOSUCHTYPE)", "42501"),
    ];
    for (sql, expected) in cases {
        let (code, text) = f.err(A, &mut s, sql);
        assert_eq!(code, expected, "{sql}: {text}");
        assert!(!text.contains('#'), "internal key leaked: {text}");
        assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "3F000", "{sql}");
        assert_eq!(
            f.code(A, &mut s, "SELECT id FROM s.t LIMIT 1"),
            "3F000",
            "{sql}"
        );
    }
    // 再オープンしても残骸が無く、同名で作り直せる。
    let f = f.reopen();
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "3F000");
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY)",
    );
    // 既存の SCHEMA は無傷。
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM keep.k LIMIT 1"), 0);
}

#[test]
fn existing_schema_name_is_42p06_and_creates_no_elements() {
    let f = fx("sch-el-exists");
    let mut s = ddl_session();
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY)",
    );
    f.ok(A, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    assert_eq!(
        f.code(A, &mut s, "CREATE SCHEMA s CREATE TABLE t2 (id BIGINT)"),
        "42P06"
    );
    assert_eq!(f.code(A, &mut s, "SELECT id FROM s.t2 LIMIT 1"), "42P01");
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.t LIMIT 10"), 1);
}

#[test]
fn tenants_are_isolated() {
    let f = fx("sch-el-tenant");
    let mut s = ddl_session();
    f.ok(
        B,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY, a INT)",
    );
    f.ok(B, &mut s, "INSERT INTO s.t (id, a) VALUES (7, 70)");
    // alice の同名 SCHEMA・同名オブジェクトは bob に影響せず、alice 側では新規扱い。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY, a INT) CREATE VIEW v AS SELECT id FROM t",
    );
    f.ok(A, &mut s, "INSERT INTO s.t (id, a) VALUES (1, 10)");
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.t LIMIT 10"), 1);
    assert_eq!(f.rows(B, &mut s, "SELECT id FROM s.t LIMIT 10"), 1);
    // bob には alice の view が見えない。
    assert_eq!(f.code(B, &mut s, "SELECT id FROM s.v LIMIT 10"), "42P01");
    // alice の既存 SCHEMA は `42P06` で、bob の行・定義は不変。
    assert_eq!(
        f.code(A, &mut s, "CREATE SCHEMA s CREATE TABLE t3 (id BIGINT)"),
        "42P06"
    );
    // 他テナントの SCHEMA 名は、存在しない名前と区別できない（要素の修飾でも同じ）。
    f.ok(
        B,
        &mut s,
        "CREATE SCHEMA bobs CREATE TABLE x (id BIGINT PRIMARY KEY)",
    );
    let ghost = f.err(
        A,
        &mut s,
        "CREATE SCHEMA a1 CREATE TABLE t (id BIGINT REFERENCES ghost.x (id))",
    );
    let other = f.err(
        A,
        &mut s,
        "CREATE SCHEMA a1 CREATE TABLE t (id BIGINT REFERENCES bobs.x (id))",
    );
    assert_eq!(ghost.0, "3F000");
    assert_eq!(other.0, "3F000");
    assert_eq!(ghost.1.replace("ghost", "X"), other.1.replace("bobs", "X"));
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA a1"), "3F000");
}

#[test]
fn ddl_gate_applies_before_any_catalog_lookup() {
    let f = fx("sch-el-perm");
    let mut allowed = ddl_session();
    f.ok(
        A,
        &mut allowed,
        "CREATE SCHEMA exists1 CREATE TABLE t (id BIGINT)",
    );
    let mut denied = SessionState::default();
    for sql in [
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT)",
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT) CREATE VIEW v AS SELECT id FROM s.nosuch",
        "CREATE SCHEMA exists1 CREATE TABLE t (id BIGINT)",
        "CREATE SCHEMA pg_x CREATE TABLE t (id BIGINT)",
        "CREATE SCHEMA s CREATE TABLE other.t (id BIGINT)",
    ] {
        let code = f.code(A, &mut denied, sql);
        // 構文段の拒否（`42P15`）は権限・状態に依存しない固定の判定。それ以外は常に `42501`。
        let expected = if sql.contains("other.t") {
            "42P15"
        } else {
            "42501"
        };
        assert_eq!(code, expected, "{sql}");
    }
    assert_eq!(
        f.code(A, &mut allowed, "SELECT id FROM s.t LIMIT 1"),
        "3F000"
    );
}

// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn unsupported_forms_are_0a000_and_malformed_forms_are_42601() {
    let f = fx("sch-el-syntax");
    let mut s = ddl_session();
    for sql in [
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE INDEX i ON t (a)",
        "CREATE SCHEMA s CREATE TABLE t (a INT) GRANT SELECT ON t TO r",
        "CREATE SCHEMA s CREATE TABLE IF NOT EXISTS t (a INT)",
        "CREATE SCHEMA IF NOT EXISTS s CREATE TABLE t (a INT)",
        "CREATE SCHEMA s CREATE TEMP TABLE t (a INT)",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE VIEW v (x) AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE VIEW v AS SELECT a FROM t WITH CHECK OPTION",
        "CREATE SCHEMA s CREATE TABLE t (a INT) CREATE OR REPLACE VIEW v AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE MATERIALIZED VIEW m AS SELECT a FROM t",
        "CREATE SCHEMA s CREATE TABLE t AS SELECT a FROM u",
    ] {
        assert_eq!(f.code(A, &mut s, sql), "0A000", "{sql}");
    }
    for sql in [
        "CREATE SCHEMA s CREATE",
        "CREATE SCHEMA s CREATE TABLE",
        "CREATE SCHEMA s CREATE TABLE t (a INT) junk",
        "CREATE SCHEMA s CREATE TABLE t (a INT) USING OPERATION_ID 'x'",
        "CREATE SCHEMA s CREATE SCHEMA u",
    ] {
        assert_eq!(f.code(A, &mut s, sql), "42601", "{sql}");
    }
    // どれも SCHEMA を残さない。
    f.ok(A, &mut s, "CREATE SCHEMA s");
}

#[test]
fn element_count_is_capped() {
    let f = fx("sch-el-cap");
    let mut s = ddl_session();
    let build = |n: usize| {
        let mut sql = String::from("CREATE SCHEMA big");
        for i in 0..n {
            sql.push_str(&format!(" CREATE SEQUENCE q{i}"));
        }
        sql
    };
    assert_eq!(f.code(A, &mut s, &build(65)), "54000");
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA big"), "3F000");
    f.ok(A, &mut s, &build(64));
    f.ok(A, &mut s, "DROP SCHEMA big CASCADE");
}

// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn explicit_transaction_is_rejected_and_creates_nothing() {
    let f = fx("sch-el-txn");
    let mut s = ddl_session();
    let mut txn = f.core.new_session_transaction();
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = f
        .core
        .execute_sql_in_txn(
            &ctx(A),
            &mut s,
            &mut txn,
            "CREATE SCHEMA s CREATE TABLE t (id BIGINT)",
        )
        .expect_err("must be rejected inside a transaction");
    assert_eq!(err.wire_code(), "0A000");
    f.core
        .execute_sql_in_txn(&ctx(A), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "3F000");
}

#[test]
fn search_path_does_not_move_elements_and_resolves_foreign_names() {
    let f = fx("sch-el-path");
    let mut s = ddl_session();
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA other CREATE TABLE base (id BIGINT PRIMARY KEY)",
    );
    f.ok(A, &mut s, "INSERT INTO other.base (id) VALUES (3)");
    f.ok(A, &mut s, "SET search_path TO other, public");
    // path の先頭が別の SCHEMA でも要素は新しい SCHEMA に作られ、本文の未修飾名は path で解決される。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE VIEW v AS SELECT id FROM base",
    );
    f.ok(A, &mut s, "INSERT INTO s.t (id) VALUES (1)");
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.v LIMIT 10"), 1);
    assert_eq!(f.code(A, &mut s, "SELECT id FROM other.t LIMIT 1"), "42P01");
    // 同じ文で作る名前が path 先の同名より優先される（新しい SCHEMA を path の先頭に置く扱い）。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s2 CREATE TABLE base (id BIGINT PRIMARY KEY, z INT) CREATE VIEW v AS SELECT z FROM base",
    );
    assert_eq!(f.rows(A, &mut s, "SELECT z FROM s2.v LIMIT 10"), 0);
}

// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn references_to_temporary_tables_are_rejected() {
    let f = fx("sch-el-temp");
    let mut s = ddl_session();
    f.ok(A, &mut s, "CREATE TEMP TABLE tt (id BIGINT)");
    assert_eq!(
        f.code(
            A,
            &mut s,
            "CREATE SCHEMA s CREATE VIEW v AS SELECT id FROM tt"
        ),
        "0A000"
    );
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s"), "3F000");
    // 一時表と同名の要素そのものは作れる（参照していなければ衝突しない）。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE tt2 (id BIGINT PRIMARY KEY)",
    );
}

// 作成中の表自身への自己 FK は、同名の一時表があっても新しい SCHEMA の表を指して受理される。
// 作成中の表以外の未修飾参照は従来どおり一時表参照として 0A000。
#[test]
fn self_fk_is_accepted_even_when_a_temp_table_has_the_same_name() {
    let f = fx("sch-el-temp-self");
    let mut s = ddl_session();
    f.ok(A, &mut s, "CREATE TEMP TABLE t (id BIGINT)");
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY, parent BIGINT REFERENCES t (id))",
    );
    assert_eq!(
        f.code(
            A,
            &mut s,
            "CREATE SCHEMA s2 CREATE TABLE u (id BIGINT PRIMARY KEY, p BIGINT REFERENCES t (id))"
        ),
        "0A000"
    );
}

#[test]
fn quoted_create_identifiers_do_not_split_elements() {
    let f = fx("sch-el-quoted");
    let mut s = ddl_session();
    // 列名 "create"・"grant" は括弧の中なので境界にならず、表は 2 つ作られる。
    f.ok(
        A,
        &mut s,
        "CREATE SCHEMA s CREATE TABLE t1 (id BIGINT PRIMARY KEY, \"grant\" INT) \
         CREATE TABLE t2 (\"create\" INT, id BIGINT PRIMARY KEY)",
    );
    f.ok(A, &mut s, "INSERT INTO s.t2 (id) VALUES (1)");
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.t2 LIMIT 10"), 1);
    assert_eq!(f.rows(A, &mut s, "SELECT id FROM s.t1 LIMIT 10"), 0);
    // 表名 "create" は境界にならず 1 つの要素として扱われる（構文語と衝突する名前を SCHEMA 配下に置く形は、
    // 単独の `CREATE TABLE s."create"` と同じ `42601`）。
    let element = f.code(
        A,
        &mut s,
        "CREATE SCHEMA s9 CREATE TABLE \"create\" (id BIGINT PRIMARY KEY)",
    );
    let single = f.code(
        A,
        &mut s,
        "CREATE TABLE s.\"create\" (id BIGINT PRIMARY KEY)",
    );
    assert_eq!(element, single);
    assert_eq!(f.code(A, &mut s, "DROP SCHEMA s9"), "3F000");
}
