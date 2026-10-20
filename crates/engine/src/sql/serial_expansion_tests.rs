//! `SERIAL` 系型の SEQUENCE・`DEFAULT nextval` への展開（TABLE-24、Issue #1694）の crate 内結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24。設計記録は
//! `docs/design/sequence-namespace-and-ledger.md`「Issue #1694 実装記録」。
//!
//! 所有者（`SequenceOwner`）はカタログ内部の宣言で SQL からは見えないため、`Storage` の内部 API で
//! 読んで検証する（外部から観測できる挙動は `TRUNCATE ... RESTART IDENTITY`・`DROP TABLE`・
//! `DROP SEQUENCE` の応答で確かめる）。検証する契約:
//!
//! - `CREATE TABLE` の `SERIAL` 列は基底整数型・`NOT NULL`・`DEFAULT nextval('<table>_<col>_seq')` と、
//!   その列が所有する暗黙 SEQUENCE へ展開され、INSERT で省略するとテナントごとに 1 から採番される
//! - 暗黙 SEQUENCE の作成は表の作成と同じ txn（途中失敗で孤児を残さない）。名前の衝突は数字を足す
//! - `DROP TABLE`・`DROP COLUMN` は所有 SEQUENCE を同じ txn で削除し、RENAME は所有者を追従させる
//! - 未対応の位置（`ADD COLUMN`・一時表・配列・`DEFAULT` 併記など）は fail-closed で拒否する

use crate::catalog::sequence::SequenceOwner;
use crate::catalog::{ColumnDefault, ColumnType};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    (path, guard, core)
}

/// `t` に 1 行入れる（`n` を省略して採番させる）。
fn insert_t(core: &EngineCore, tenant: &str, id: u64, op: &str) {
    ok(
        core,
        tenant,
        &format!("INSERT INTO t (id, v) VALUES ({id}, 'x') USING OPERATION_ID '{op}'"),
    );
}

/// `t.n` を id 昇順で返す（INTEGER／SMALLINT／BIGINT いずれも `SignedInteger`）。
fn column_n(core: &EngineCore, tenant: &str) -> Vec<i64> {
    let r = core
        .execute_sql(&ctx(tenant), "SELECT id, n FROM t LIMIT 1000")
        .expect("select");
    let mut rows: Vec<(u64, i64)> = r
        .rows
        .iter()
        .map(|row| {
            let id = match row.cells[0] {
                Cell::Integer(v) => v,
                ref o => panic!("id cell {o:?}"),
            };
            let n = match row.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n cell {o:?}"),
            };
            (id, n)
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, n)| n).collect()
}

fn owner(table: &str, column: &str) -> Option<SequenceOwner> {
    Some(SequenceOwner {
        table: table.to_string(),
        column: column.to_string(),
    })
}

/// 受け入れ条件 1・3: 展開結果（型・NOT NULL・DEFAULT・所有者・パラメータ）と、テナントごとの 1 始まりの採番。
#[test]
fn serial_expands_and_numbers_per_tenant_from_one() {
    let (_p, _g, core) = fixture("serial-basic");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");

    let storage = core.storage_for_test();
    let schema = storage.get_table_schema("t").expect("schema");
    let col = schema.columns.iter().find(|c| c.name == "n").expect("n");
    assert_eq!(col.ty, ColumnType::Integer);
    assert!(!col.nullable);
    assert_eq!(
        col.default_expr,
        Some(ColumnDefault::Sequence("t_n_seq".to_string()))
    );
    assert_eq!(
        storage.sequence_owner("t_n_seq").expect("owner"),
        owner("t", "n")
    );
    let def = storage
        .sequence_definition_full("t_n_seq")
        .expect("def")
        .expect("exists");
    assert_eq!(def.params.increment, 1);
    assert_eq!(def.params.min_value, 1);
    assert_eq!(def.params.start, 1);
    assert_eq!(def.params.max_value, i64::from(i32::MAX));
    assert!(!def.params.cycle);

    insert_t(&core, "alice", 1, "a1");
    insert_t(&core, "alice", 2, "a2");
    insert_t(&core, "bob", 1, "b1");
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
    assert_eq!(column_n(&core, "bob"), vec![1]);
    insert_t(&core, "bob", 2, "b2");
    insert_t(&core, "alice", 3, "a3");
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
    assert_eq!(column_n(&core, "bob"), vec![1, 2]);
}

/// 6 つの綴りの基底型・上限・大文字小文字無視。
#[test]
fn serial_spellings_map_to_base_types_and_limits() {
    let (_p, _g, core) = fixture("serial-spellings");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (a SERIAL, b serial4, c BIGSERIAL, d Serial8, e SMALLSERIAL, f serial2, v TEXT)",
    );
    let schema = core
        .storage_for_test()
        .get_table_schema("t")
        .expect("schema");
    let expect = [
        ("a", ColumnType::Integer, i64::from(i32::MAX)),
        ("b", ColumnType::Integer, i64::from(i32::MAX)),
        ("c", ColumnType::BigInt, i64::MAX),
        ("d", ColumnType::BigInt, i64::MAX),
        ("e", ColumnType::SmallInt, i64::from(i16::MAX)),
        ("f", ColumnType::SmallInt, i64::from(i16::MAX)),
    ];
    for (name, ty, max) in expect {
        let col = schema.columns.iter().find(|c| c.name == name).expect("col");
        assert_eq!(col.ty, ty, "{name}");
        assert!(!col.nullable, "{name}");
        let seq = format!("t_{name}_seq");
        assert_eq!(
            col.default_expr,
            Some(ColumnDefault::Sequence(seq.clone())),
            "{name}"
        );
        let def = core
            .storage_for_test()
            .sequence_definition_full(&seq)
            .expect("def")
            .expect("exists");
        assert_eq!(def.params.max_value, max, "{name}");
    }
}

/// SMALLSERIAL の上限は SEQUENCE の `2200H`（列に入らない値の `22003` ではない）。
#[test]
fn smallserial_exhaustion_is_sequence_limit_error() {
    let (_p, _g, core) = fixture("serial-small-limit");
    ok(&core, "sys", "CREATE TABLE t (n SMALLSERIAL, v TEXT)");
    ok(&core, "alice", "SELECT setval('t_n_seq', 32766)");
    insert_t(&core, "alice", 1, "a1");
    assert_eq!(column_n(&core, "alice"), vec![32767]);
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, v) VALUES (2, 'x') USING OPERATION_ID 'a2'"
        ),
        "2200H"
    );
}

/// 明示値は受理（BY DEFAULT 相当）、明示 NULL は `23502`。`NOT NULL`・`PRIMARY KEY`・`UNIQUE` は併記できる。
#[test]
fn serial_accepts_explicit_values_and_rejects_null() {
    let (_p, _g, core) = fixture("serial-explicit");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (n SERIAL NOT NULL UNIQUE, v TEXT)",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n, v) VALUES (1, 100, 'x') USING OPERATION_ID 'a1'",
    );
    insert_t(&core, "alice", 2, "a2");
    assert_eq!(column_n(&core, "alice"), vec![100, 1]);
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, n, v) VALUES (3, NULL, 'x') USING OPERATION_ID 'a3'"
        ),
        "23502"
    );
    ok(
        &core,
        "sys",
        "CREATE TABLE p (k BIGSERIAL PRIMARY KEY, v TEXT)",
    );
}

/// 暗黙 SEQUENCE は通常の SEQUENCE として見える（名前空間・依存・`setval`）。
#[test]
fn implicit_sequence_behaves_as_owned_sequence() {
    let (_p, _g, core) = fixture("serial-visible");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    assert_eq!(code(&core, "sys", "CREATE SEQUENCE t_n_seq"), "42P07");
    assert_eq!(
        code(&core, "sys", "CREATE TABLE t_n_seq (a BIGINT)"),
        "42P07"
    );
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "2BP01");
    ok(&core, "alice", "SELECT setval('t_n_seq', 10)");
    insert_t(&core, "alice", 1, "a1");
    assert_eq!(column_n(&core, "alice"), vec![11]);
}

/// 名前の衝突は数字の接尾辞で避ける（PostgreSQL の `ChooseRelationName`）。
#[test]
fn name_collision_appends_numeric_suffix() {
    let (_p, _g, core) = fixture("serial-name-collision");
    ok(&core, "sys", "CREATE SEQUENCE t_n_seq");
    ok(&core, "sys", "CREATE TABLE t_n_seq1 (a BIGINT)");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    let schema = core
        .storage_for_test()
        .get_table_schema("t")
        .expect("schema");
    let col = schema.columns.iter().find(|c| c.name == "n").expect("n");
    assert_eq!(
        col.default_expr,
        Some(ColumnDefault::Sequence("t_n_seq2".to_string()))
    );
    // 既存の SEQUENCE・表は所有されず、そのまま残る。
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq")
            .expect("o"),
        None
    );
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq2")
            .expect("o"),
        owner("t", "n")
    );
}

/// 同じ文の複数列で名前が衝突しても、先に決めた名前を避ける。識別子長上限（63 バイト）へ切り詰める。
#[test]
fn long_names_are_truncated_and_same_statement_collisions_are_resolved() {
    let (_p, _g, core) = fixture("serial-long-names");
    let table = "t".repeat(40);
    let c1 = "x".repeat(40);
    let c2 = format!("{c1}y");
    ok(
        &core,
        "sys",
        &format!("CREATE TABLE {table} ({c1} SERIAL, {c2} SERIAL, v TEXT)"),
    );
    let schema = core
        .storage_for_test()
        .get_table_schema(&table)
        .expect("schema");
    let names: Vec<String> = schema
        .columns
        .iter()
        .filter_map(|c| match &c.default_expr {
            Some(ColumnDefault::Sequence(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    for n in &names {
        assert!(n.len() <= 63, "{n}");
        assert!(n.starts_with('t'), "{n}");
        assert_eq!(
            core.storage_for_test().sequence_owner(n).expect("owner"),
            owner(&table, if n == &names[0] { &c1 } else { &c2 }),
        );
    }
    assert!(names[0].ends_with("_seq") && names[1].ends_with("_seq1"));
}

/// 受け入れ条件 2: DROP TABLE で所有 SEQUENCE も同じ txn で消える。再作成した表の採番は 1 から。
#[test]
fn drop_table_drops_owned_sequence_and_state() {
    let (_p, _g, core) = fixture("serial-drop-table");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    insert_t(&core, "alice", 1, "a1");
    insert_t(&core, "alice", 2, "a2");
    ok(&core, "sys", "DROP TABLE t");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
    ok(&core, "sys", "CREATE SEQUENCE t_n_seq");
    ok(&core, "sys", "DROP SEQUENCE t_n_seq");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    insert_t(&core, "alice", 1, "a3");
    assert_eq!(column_n(&core, "alice"), vec![1]);
}

/// 削除対象外の表の DEFAULT が所有 SEQUENCE を参照していれば `2BP01`（RESTRICT・CASCADE とも）で、
/// 何も削除しない。
#[test]
fn drop_table_with_dependent_default_is_refused() {
    let (_p, _g, core) = fixture("serial-drop-dependent");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    ok(
        &core,
        "sys",
        "CREATE TABLE u (m BIGINT DEFAULT nextval('t_n_seq'), w TEXT)",
    );
    assert_eq!(code(&core, "sys", "DROP TABLE t"), "2BP01");
    assert_eq!(code(&core, "sys", "DROP TABLE t CASCADE"), "2BP01");
    // 何も消えていない（表も SEQUENCE も採番も）。
    insert_t(&core, "alice", 1, "a1");
    assert_eq!(column_n(&core, "alice"), vec![1]);
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq")
            .expect("o"),
        owner("t", "n")
    );
    // 同じ文で両方を消すなら依存は残らない。
    ok(&core, "sys", "DROP TABLE t, u");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// RENAME は所有者を追従させる。改名後の表で `RESTART IDENTITY` が採番を戻し、DROP TABLE が SEQUENCE を消す。
#[test]
fn rename_table_and_column_keep_ownership() {
    let (_p, _g, core) = fixture("serial-rename");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    insert_t(&core, "alice", 1, "a1");
    insert_t(&core, "alice", 2, "a2");
    ok(&core, "sys", "ALTER TABLE t RENAME COLUMN n TO k");
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq")
            .expect("o"),
        owner("t", "k")
    );
    ok(&core, "sys", "ALTER TABLE t RENAME TO t2");
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq")
            .expect("o"),
        owner("t2", "k")
    );
    // 改名後の表で RESTART IDENTITY が効く（#1645 との結線）。
    ok(
        &core,
        "alice",
        "TRUNCATE t2 RESTART IDENTITY USING OPERATION_ID 'r1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t2 (id, v) VALUES (1, 'x') USING OPERATION_ID 'a3'",
    );
    let r = core
        .execute_sql(&ctx("alice"), "SELECT k FROM t2 LIMIT 10")
        .expect("select");
    assert_eq!(r.rows.len(), 1);
    assert!(matches!(r.rows[0].cells[0], Cell::SignedInteger(1)));
    ok(&core, "sys", "DROP TABLE t2");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// SERIAL 列の DROP COLUMN は所有 SEQUENCE も消す。
#[test]
fn drop_column_drops_owned_sequence() {
    let (_p, _g, core) = fixture("serial-drop-column");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    ok(&core, "sys", "ALTER TABLE t DROP COLUMN n");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// 複数アクションで SERIAL 列を落とし別の列を足せる（DROP 直後は生存列 0 本の中間状態。
/// 依存走査がその中間状態を読めること）。所有 SEQUENCE は消え、カーソルも残らない。
#[test]
fn multi_action_drop_serial_column_and_add_column() {
    let (_p, _g, core) = fixture("serial-multi-drop");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL)");
    ok(
        &core,
        "sys",
        "ALTER TABLE t DROP COLUMN n, ADD COLUMN v TEXT",
    );
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// 複数アクションで、所有 SEQUENCE を参照する別列の DROP COLUMN と同じ文に書けば、記述順によらず
/// 依存は残らず通る。DROP DEFAULT を併用しても通り、依存が残る場合は 2BP01 で何も適用しない。
#[test]
fn multi_action_drop_checks_dependents_after_all_actions() {
    for (i, alter) in [
        "ALTER TABLE t DROP COLUMN n, DROP COLUMN m",
        "ALTER TABLE t DROP COLUMN m, DROP COLUMN n",
        "ALTER TABLE t DROP COLUMN n, ALTER COLUMN m DROP DEFAULT",
        "ALTER TABLE t ALTER COLUMN m DROP DEFAULT, DROP COLUMN n",
    ]
    .iter()
    .enumerate()
    {
        let (_p, _g, core) = fixture(&format!("serial-multi-dep-{i}"));
        ok(
            &core,
            "sys",
            "CREATE TABLE t (n SERIAL, m BIGINT DEFAULT nextval('t_n_seq'), v TEXT)",
        );
        ok(&core, "sys", alter);
        assert_eq!(
            code(&core, "sys", "DROP SEQUENCE t_n_seq"),
            "42P01",
            "{alter}"
        );
    }
    // 依存が残る場合は拒否され、何も適用されない。
    let (_p, _g, core) = fixture("serial-multi-dep-kept");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (n SERIAL, m BIGINT DEFAULT nextval('t_n_seq'), v TEXT)",
    );
    assert_eq!(
        code(&core, "sys", "ALTER TABLE t DROP COLUMN n, DROP COLUMN v"),
        "2BP01"
    );
    insert_t(&core, "alice", 1, "a1");
    assert_eq!(column_n(&core, "alice"), vec![1]);
}

/// 単独・複数アクションの DROP COLUMN は、所有 SEQUENCE のメモリ上のカーソルも commit 後に捨てる。
#[test]
fn drop_column_evicts_sequence_cursors() {
    let (_p, _g, core) = fixture("serial-evict");
    for (i, ddl) in [
        "ALTER TABLE t DROP COLUMN n",
        "ALTER TABLE t DROP COLUMN n, ADD COLUMN w TEXT",
    ]
    .iter()
    .enumerate()
    {
        ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
        ok(
            &core,
            "alice",
            &format!("INSERT INTO t (id, v) VALUES (1, 'x') USING OPERATION_ID 'e{i}'"),
        );
        let before = core.storage_for_test().sequence_allocator().cursor_count();
        assert!(before >= 1, "cursor must exist after nextval");
        ok(&core, "sys", ddl);
        assert_eq!(
            core.storage_for_test().sequence_allocator().cursor_count(),
            0
        );
        ok(&core, "sys", "DROP TABLE t");
    }
}

/// `IF NOT EXISTS` で表が既にあれば暗黙 SEQUENCE も作らない。
#[test]
fn if_not_exists_existing_table_creates_no_sequence() {
    let (_p, _g, core) = fixture("serial-if-not-exists");
    ok(&core, "sys", "CREATE TABLE t (v TEXT)");
    ok(
        &core,
        "sys",
        "CREATE TABLE IF NOT EXISTS t (n SERIAL, v TEXT)",
    );
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
    assert_eq!(code(&core, "sys", "CREATE TABLE t (n SERIAL)"), "42P07");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// 表の作成が後段で失敗しても（FK の参照先不在）、暗黙 SEQUENCE は残らない（単一 txn の原子性）。
#[test]
fn failed_create_table_leaves_no_orphan_sequence() {
    let (_p, _g, core) = fixture("serial-atomic");
    assert_eq!(
        code(
            &core,
            "sys",
            "CREATE TABLE t (n SERIAL, r BIGINT REFERENCES nowhere)"
        ),
        "42P01"
    );
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
}

/// 再オープン後も定義・所有者・採番の続きが保たれる。
#[test]
fn serial_state_survives_reopen() {
    let (path, _g, core) = fixture("serial-reopen");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    insert_t(&core, "alice", 1, "a1");
    insert_t(&core, "alice", 2, "a2");
    drop(core);
    let core = open_core(&path);
    assert_eq!(
        core.storage_for_test()
            .sequence_owner("t_n_seq")
            .expect("o"),
        owner("t", "n")
    );
    insert_t(&core, "alice", 3, "a3");
    assert_eq!(column_n(&core, "alice"), vec![1, 2, 3]);
}

/// 台帳: 同じ `operation_id` の再送は `23505` で、値を消費しない。
#[test]
fn ledger_replay_does_not_consume_serial_value() {
    let (_p, _g, core) = fixture("serial-ledger");
    ok(&core, "sys", "CREATE TABLE t (n SERIAL, v TEXT)");
    insert_t(&core, "alice", 1, "a1");
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, v) VALUES (1, 'x') USING OPERATION_ID 'a1'"
        ),
        "23505"
    );
    insert_t(&core, "alice", 2, "a2");
    assert_eq!(column_n(&core, "alice"), vec![1, 2]);
}

/// 未対応の形は fail-closed で SQLSTATE を固定して拒否し、SEQUENCE も表も作らない。
#[test]
fn unsupported_forms_are_rejected_without_side_effects() {
    let (_p, _g, core) = fixture("serial-reject");
    for (sql, expected) in [
        ("CREATE TABLE r (n SERIAL DEFAULT 1)", "42601"),
        ("CREATE TABLE r (n SERIAL DEFAULT NULL)", "42601"),
        ("CREATE TABLE r (n SERIAL DEFAULT now())", "42601"),
        ("CREATE TABLE r (n SERIAL[])", "0A000"),
        ("CREATE TABLE r (n SERIAL[3])", "0A000"),
        ("CREATE TEMP TABLE r (n SERIAL)", "0A000"),
        ("CREATE TABLE r (id SERIAL)", "42601"),
    ] {
        assert_eq!(code(&core, "sys", sql), expected, "{sql}");
    }
    assert_eq!(code(&core, "sys", "DROP SEQUENCE r_n_seq"), "42P01");
    assert_eq!(code(&core, "sys", "DROP TABLE r"), "42P01");

    ok(&core, "sys", "CREATE TABLE t (v TEXT)");
    for (sql, expected) in [
        ("ALTER TABLE t ADD COLUMN m SERIAL", "0A000"),
        ("ALTER TABLE t ADD COLUMN m BIGSERIAL", "0A000"),
        ("ALTER TABLE t ALTER COLUMN v TYPE serial", "42704"),
    ] {
        assert_eq!(code(&core, "sys", sql), expected, "{sql}");
    }
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_m_seq"), "42P01");
}

/// DDL 権限の無いセッションは `42501` で、SEQUENCE も作られない。
#[test]
fn serial_requires_ddl_permission() {
    let (_p, _g, core) = fixture("serial-no-ddl");
    let mut session = SessionState::default();
    let e = core
        .execute_sql_in_session(&ctx("alice"), &mut session, "CREATE TABLE t (n SERIAL)")
        .expect_err("must fail");
    assert_eq!(e.wire_code(), "42501");
    assert_eq!(code(&core, "sys", "DROP SEQUENCE t_n_seq"), "42P01");
}

/// 列定義の外では SERIAL 系は型として存在しない（`42704`）。
#[test]
fn serial_is_not_a_type_outside_column_definitions() {
    let (_p, _g, core) = fixture("serial-cast");
    ok(&core, "sys", "CREATE TABLE t (v TEXT)");
    let e = core
        .execute_sql(&ctx("alice"), "SELECT CAST(v AS serial) FROM t LIMIT 1")
        .expect_err("must fail");
    assert_eq!(e.wire_code(), "42704");
}

/// 予約語: SERIAL 系は CHECK 制約名・NoSQL の ENUM 型名として拒否される。
#[test]
fn serial_spellings_are_column_type_keywords() {
    for kw in [
        "serial",
        "SERIAL4",
        "bigserial",
        "SERIAL8",
        "smallserial",
        "SERIAL2",
    ] {
        assert!(crate::sql::allowlist::is_column_type_keyword(kw), "{kw}");
    }
}
