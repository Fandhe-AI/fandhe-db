//! UDF 件数上限の起動時設定と DDL 権限の整合の結合テスト（Issue #1922、
//! 対象ビヘイビア: SQL-66 (5)・関連 SQL-46・SQL-9 のポインタ。設計記録: `docs/design/udf-catalog.md`）。
//!
//! 固定する契約: 設定した上限が権限ありのカタログ経路で効く（既定は 64）／件数はテナントごと／
//! 重複 `42723` が上限 `54000` より先／上限を下げて再オープンしても既存の関数はロードされ呼べる／
//! 権限の無いセッションの `CREATE`（`OR REPLACE`・型付き引数・RETURNS を含む）と `DROP FUNCTION` は
//! カタログにもセッション内レジストリにも触れず `42501`（保存先の振り分けより前に拒否）。権限ありの
//! 型付き引数・RETURNS 付きも同じカタログ経路で永続化され、上限・判定順が効く（Issue #2798）。永続定義が
//! セッション限定の関数（カタログに載らない宣言的 UDF・WASM UDF）へ依存する形は `0A000`。

use engine::core::EngineCore;
use engine::function_limits::FunctionLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::udf_call::{define_function, Expr};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(path: &std::path::Path, max_functions: usize) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger)
    .with_function_limits(FunctionLimits::new(max_functions))
}

fn session(ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    if ddl {
        s.allow_ddl();
    }
    s
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, (String, String)> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .map_err(|e| (e.wire_code().to_string(), e.to_string()))
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    run(core, tenant, s, sql).unwrap_or_else(|(c, m)| panic!("{sql}: {c} {m}"));
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match run(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err((c, _)) => c,
    }
}

fn create(n: usize) -> String {
    format!("CREATE FUNCTION f{n}(x) AS x + {n}")
}

/// T1: 権限ありの経路で設定した上限が効き、件数はテナントごとに数える。
#[test]
fn configured_limit_applies_to_catalog_path_per_tenant() {
    let path = unique_db_path("sql66-limit-catalog");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut a = session(true);
    ok(&core, "alice", &mut a, &create(1));
    ok(&core, "alice", &mut a, &create(2));
    assert_eq!(code(&core, "alice", &mut a, &create(3)), "54000");
    // 他テナントは自分の分を 2 件まで作れる。
    let mut b = session(true);
    ok(&core, "bob", &mut b, &create(1));
    ok(&core, "bob", &mut b, &create(2));
    assert_eq!(code(&core, "bob", &mut b, &create(3)), "54000");
}

/// T2: 既定の上限は 64（設定なしの `EngineCore`）。
#[test]
fn default_limit_is_64() {
    let path = unique_db_path("sql66-limit-default");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let mut s = session(true);
    for n in 0..64 {
        ok(&core, "alice", &mut s, &create(n));
    }
    assert_eq!(code(&core, "alice", &mut s, &create(64)), "54000");
}

/// T3: 上限に達していても同名の再定義は 42723（重複が上限より先に判定される）。
#[test]
fn duplicate_is_reported_before_limit() {
    let path = unique_db_path("sql66-limit-order");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 1);
    let mut s = session(true);
    ok(&core, "alice", &mut s, &create(1));
    assert_eq!(code(&core, "alice", &mut s, &create(1)), "42723");
}

/// T4: 上限を下げて再オープンしても既存の関数はロードされ呼べる。新規作成だけが拒否される。
#[test]
fn lowering_the_limit_keeps_existing_functions_usable() {
    let path = unique_db_path("sql66-limit-lowered");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path, 64);
        let mut s = session(true);
        ok(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
        );
        ok(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (1, 5)",
        );
        for n in 1..=3 {
            ok(&core, "alice", &mut s, &create(n));
        }
    }
    let core = open(&path, 1);
    let mut s = session(true);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    for n in 1..=3 {
        ok(
            &core,
            "alice",
            &mut s,
            &format!("SELECT f{n}(n) AS d FROM t LIMIT 1"),
        );
    }
    assert_eq!(code(&core, "alice", &mut s, &create(4)), "54000");
    for n in 1..=3 {
        ok(&core, "alice", &mut s, &format!("DROP FUNCTION f{n}"));
    }
    ok(&core, "alice", &mut s, &create(4));
}

/// T5: 権限の無いセッションの CREATE／CREATE OR REPLACE／型付き引数／RETURNS／DROP はすべて `42501`
/// で、カタログ・セッション内レジストリ・件数のいずれも変わらない（受け入れ条件「権限なしは拒否」）。
/// 権限検査は保存先の振り分け（カタログ・セッション限定）より前に効く。
#[test]
fn session_without_permission_is_rejected_and_nothing_changes() {
    let path = unique_db_path("sql66-limit-perm");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut d = session(true);
    ok(&core, "alice", &mut d, &create(1));
    let mut s = session(false);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    assert_eq!(s.udfs().len(), 1);
    for sql in [
        create(2),
        "CREATE OR REPLACE FUNCTION f2(x) AS x + 2".to_string(),
        "CREATE OR REPLACE FUNCTION f1(x) AS x + 100".to_string(),
        "CREATE FUNCTION f3(x integer) AS x + 3".to_string(),
        "CREATE OR REPLACE FUNCTION f3(x integer) AS x + 3".to_string(),
        "CREATE FUNCTION f4() RETURNS integer AS 4".to_string(),
        "DROP FUNCTION f1".to_string(),
        "DROP FUNCTION IF EXISTS f1".to_string(),
        "DROP FUNCTION IF EXISTS never_defined".to_string(),
        "DROP FUNCTION never_defined".to_string(),
    ] {
        assert_eq!(code(&core, "alice", &mut s, &sql), "42501", "{sql}");
        assert_eq!(s.udfs().len(), 1, "registry must not change: {sql}");
    }
    // カタログ: f1 は残り、件数は 1 のまま（上限 2 のもとで 1 件だけ追加できる）。
    let mut d2 = session(true);
    core.load_session_udfs(&ctx("alice"), &mut d2)
        .expect("load session udfs");
    assert_eq!(d2.udfs().len(), 1);
    ok(&core, "alice", &mut d2, &create(2));
    assert_eq!(code(&core, "alice", &mut d2, &create(3)), "54000");
    assert_eq!(code(&core, "alice", &mut d2, &create(1)), "42723");
}

/// T5b: 権限の無いセッションは明示トランザクション内でも `42501`（`0A000` より先に権限を検査する）。
#[test]
fn create_and_drop_in_explicit_transaction_without_permission_are_42501() {
    let path = unique_db_path("sql66-limit-perm-txn");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut s = session(false);
    let alice = ctx("alice");
    for sql in [create(1), "DROP FUNCTION f1".to_string()] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&alice, &mut s, &mut txn, &sql)
            .expect_err("rejected");
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
}

/// T7: 権限ありの型付き引数・RETURNS 付き定義も、カタログへ永続化され上限・判定順（重複 `42723` →
/// 件数 `54000`）が効く。別セッションから見え、`DROP FUNCTION` で枠が空く。
#[test]
fn typed_and_returns_definitions_are_persisted_with_limit_and_drop() {
    let path = unique_db_path("sql66-limit-typed-persisted");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut s = session(true);
    ok(&core, "alice", &mut s, "CREATE FUNCTION t1(x integer) AS x");
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE FUNCTION t2() RETURNS integer AS 2",
    );
    // 重複は件数上限より先に 42723、超過は 54000。失敗時はレジストリが変わらない。
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION t1(x integer) AS x"),
        "42723"
    );
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION t3(x integer) AS x"),
        "54000"
    );
    assert_eq!(s.udfs().len(), 2);
    // 別セッションにロードされ、同じカタログ件数で数えられる（上限に達している）。
    let mut other = session(true);
    core.load_session_udfs(&ctx("alice"), &mut other)
        .expect("load session udfs");
    assert_eq!(other.udfs().len(), 2);
    assert_eq!(code(&core, "alice", &mut other, &create(1)), "54000");
    // 別テナントには見えず、枠も独立。
    let mut bob = session(true);
    ok(&core, "bob", &mut bob, "CREATE FUNCTION t1(x integer) AS x");
    // DROP でカタログから消え、枠が空く。
    ok(&core, "alice", &mut s, "DROP FUNCTION t1");
    ok(&core, "alice", &mut s, "CREATE FUNCTION t3(x integer) AS x");
    // 権限なしは状態不変で 42501（T5 と同じ）。
    let mut n = session(false);
    assert_eq!(
        code(&core, "alice", &mut n, "CREATE FUNCTION t9(x integer) AS x"),
        "42501"
    );
    assert_eq!(n.udfs().len(), 0);
    // 再起動後も型付き・RETURNS 付きの定義がロードされ、呼べる。
    drop(core);
    let core = open(&path, 2);
    let mut r = session(false);
    core.load_session_udfs(&ctx("alice"), &mut r)
        .expect("load session udfs");
    assert!(r.udfs().get("t2").is_some());
    assert!(r.udfs().get("t3").is_some());
}

/// T6: 権限ありの明示トランザクション内の CREATE FUNCTION は上限の設定に関係なく 0A000。
#[test]
fn create_in_explicit_transaction_stays_rejected() {
    let path = unique_db_path("sql66-limit-txn");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 2);
    let mut s = session(true);
    let alice = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&alice, &mut s, &mut txn, &create(1))
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "0A000");
}

/// T7: 既存関数の置換は設定上限を下げた DB でも通り（件数が増えない）、新規作成だけが 54000 になる。
#[test]
fn replacing_existing_function_skips_the_configured_limit() {
    let path = unique_db_path("sql66-limit-replace");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path, 64);
        let mut s = session(true);
        for n in 1..=3 {
            ok(&core, "alice", &mut s, &create(n));
        }
    }
    let core = open(&path, 1);
    let mut s = session(true);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    // カタログ経路: 既存の置換は通り、新規は 54000。
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE OR REPLACE FUNCTION f2(x) AS x + 100",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION f4(x) AS x"
        ),
        "54000"
    );
}

/// T8: 永続定義はセッション限定の関数へ直接・推移的に依存できない（0A000）。OR REPLACE も同じ。
/// セッション限定がカタログ関数を呼ぶのは許可。拒否後はカタログも変わらず、別接続は 42883 にならない。
#[test]
fn persistent_function_cannot_depend_on_session_local_function() {
    let path = unique_db_path("sql66-persist-dep");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 8);
    let mut s = session(true);
    // セッション限定の定義は SQL では作れない（権限ありの CREATE はすべて永続化する）ため、公開 API の
    // `define_function`（カタログ由来の印を付けない登録）で用意する。
    define_function(
        s.udfs_mut(),
        "loc",
        &["x".to_string()],
        &Expr::Ident("x".to_string()),
    )
    .expect("define loc");
    // 直接依存・推移的依存（セッション限定 → セッション限定）。
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION p1(x) AS loc(x)"),
        "0A000"
    );
    define_function(
        s.udfs_mut(),
        "loc2",
        &["x".to_string()],
        &Expr::Call {
            name: "loc".to_string(),
            args: vec![Expr::Ident("x".to_string())],
        },
    )
    .expect("define loc2");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE FUNCTION p2(x) AS loc2(x) * 2"
        ),
        "0A000"
    );
    // OR REPLACE でカタログ関数をセッション限定への依存へ置き換えるのも拒否。
    ok(&core, "alice", &mut s, "CREATE FUNCTION p3(x) AS x + 1");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "CREATE OR REPLACE FUNCTION p3(x) AS loc(x)"
        ),
        "0A000"
    );
    // 逆向き: セッション限定がカタログ関数を呼ぶのは通る。
    define_function(
        s.udfs_mut(),
        "loc3",
        &["x".to_string()],
        &Expr::Call {
            name: "p3".to_string(),
            args: vec![Expr::Ident("x".to_string())],
        },
    )
    .expect("define loc3");
    // 永続定義どうしの依存は通る。
    ok(&core, "alice", &mut s, "CREATE FUNCTION p4(x) AS p3(x) + 1");
    // 別接続では p3 は元の定義のまま、p1・p2 は存在しない。
    let mut other = session(true);
    core.load_session_udfs(&ctx("alice"), &mut other)
        .expect("load session udfs");
    assert!(other.udfs().get("p3").is_some());
    assert!(other.udfs().get("p4").is_some());
    assert!(other.udfs().get("p1").is_none());
    assert!(other.udfs().get("p2").is_none());
}

/// T8b: SCHEMA 修飾で呼ぶセッション限定の関数（内部キー `#<id>.<name>` で登録）も依存として拒否する。
/// 呼び出し時と同じ名前の引き方（`UdfRegistry::get`・`get_wasm`）で依存を解決するため、修飾の有無で
/// 検査が素通りしない。未修飾の `loc` は呼び出し時にも解決されない（`search_path` は関数名に効かない）
/// ため依存にならず、作成は従来どおり 42883。
#[test]
fn persistent_function_dependency_follows_call_time_name_resolution() {
    let path = unique_db_path("sql66-persist-dep-schema");
    let _g = CleanupGuard(path.clone());
    let core = open(&path, 8);
    let mut s = session(true);
    ok(&core, "alice", &mut s, "CREATE SCHEMA s");
    // SCHEMA の内部 ID は公開されないため、候補 ID すべてへセッション限定の関数を登録する。
    for n in 1..8 {
        let _ = define_function(
            s.udfs_mut(),
            &format!("#{n}.loc"),
            &["x".to_string()],
            &Expr::Ident("x".to_string()),
        );
    }
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION p1(x) AS s.loc(x)"),
        "0A000"
    );
    ok(&core, "alice", &mut s, "SET search_path = s, public");
    assert_eq!(
        code(&core, "alice", &mut s, "CREATE FUNCTION p3(x) AS loc(x)"),
        "42883"
    );
    let mut other = session(true);
    core.load_session_udfs(&ctx("alice"), &mut other)
        .expect("load session udfs");
    for n in ["p1", "p3"] {
        assert!(other.udfs().get(n).is_none(), "{n}");
    }
}

/// 明示トランザクション（BEGIN → `sql` → ROLLBACK）で `sql` を実行し、エラーの wire_code を返す。
fn code_in_txn(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    let c = ctx(tenant);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&c, s, &mut txn, sql)
        .expect_err("persistent function DDL inside a transaction must be rejected");
    core.execute_sql_in_txn(&c, s, &mut txn, "ROLLBACK")
        .expect("rollback");
    err.wire_code().to_string()
}

/// T9: 明示トランザクション内の永続 DDL は、型注釈・RETURNS の有無によらず 0A000（権限なしは 42501 が先）。
/// ROLLBACK 後も別セッション・再起動後に関数は存在せず、OR REPLACE も取り消し不要（変更されない）。
#[test]
fn persistent_function_ddl_in_explicit_transaction_is_rejected() {
    let path = unique_db_path("sql66-txn-ddl");
    let _g = CleanupGuard(path.clone());
    let sqls = [
        "CREATE FUNCTION t1(x integer) AS x",
        "CREATE FUNCTION t3(x integer) RETURNS integer AS x",
        "CREATE FUNCTION t4(x) AS x",
        "CREATE OR REPLACE FUNCTION keep(x integer) RETURNS integer AS x + 100",
        "CREATE OR REPLACE FUNCTION t5(x integer) AS x",
        "DROP FUNCTION keep",
    ];
    {
        let core = open(&path, 8);
        let mut s = session(true);
        ok(&core, "alice", &mut s, "CREATE FUNCTION keep(x) AS x + 1");
        for sql in sqls {
            assert_eq!(code_in_txn(&core, "alice", &mut s, sql), "0A000", "{sql}");
        }
        // 権限なしは 42501 が先。
        let mut nd = session(false);
        for sql in sqls {
            assert_eq!(code_in_txn(&core, "alice", &mut nd, sql), "42501", "{sql}");
        }
        let mut other = session(true);
        core.load_session_udfs(&ctx("alice"), &mut other)
            .expect("load session udfs");
        assert!(other.udfs().get("keep").is_some());
        for n in ["t1", "t3", "t4", "t5"] {
            assert!(other.udfs().get(n).is_none(), "{n}");
        }
    }
    let core = open(&path, 8);
    let mut s = session(true);
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    assert!(s.udfs().get("keep").is_some());
    for n in ["t1", "t3", "t4", "t5"] {
        assert!(s.udfs().get(n).is_none(), "{n}");
    }
}
