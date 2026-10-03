//! `CREATE TABLE <table> (<col> <type>[, ...]) [;]`（SQL-23・TASK-85・TASK-202、
//! Issue #899）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-85・
//! TASK-202・`docs/spec/04-behavior/sql-surface.md` SQL-23・
//! `docs/spec/04-behavior/table-model.md` TABLE-1・TABLE-2・TABLE-4・TABLE-6。
//!
//! `EngineCore::execute_sql_in_session`（`parse_tokens` による `CREATE TABLE`
//! の構造検証〔`sql::allowlist::validate_create_table_tokens`〕→
//! `execute_parsed_in_session` の `ParsedSql::CreateTable` 分岐が
//! `sql::ddl::require_ddl_permission`〔DDL 実行権限ゲート。`DROP TABLE` と共有
//! する単一の判定点〕→ `sql::ddl::execute_create_table`〔`catalog::Storage::
//! create_table` への委譲〕の順に適用する経路）を production 経路として検証
//! する。`truncate_table.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// SQL 表層の行形 INSERT は既定で `Visibility::Private` の行を作る
/// （`sql_operation_id.rs` と同じ流儀）。自分が書いた行を読み戻すテストのため
/// `Public`／`Private` の両方を許可するコンテキストを既定にする。
fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

// --- 成功系 -----------------------------------------------------------

#[test]
fn create_table_succeeds_with_vector_and_text_columns() {
    let (core, path) = new_core("create-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), body TEXT)",
        )
        .expect("CREATE TABLE should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));

    // `EngineCore` はカタログを直接公開しないため、列の型・nullable 既定を
    // 機能的に確認する（2.2「`VECTOR` 列は常に non-null・`TEXT` 列は nullable」）。
    // `body`（nullable）を省略した INSERT は成功する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding) VALUES (1, '[0.1,0.2,0.3,0.4]') USING OPERATION_ID 'op-omit-body'",
    )
    .expect("omitting the nullable TEXT column must succeed");
    // `embedding`（non-null）を省略した INSERT は `23502`（`NotNullViolation`。
    // TABLE-16・TASK-204、Issue #904。旧 `22000` から契約変更）で拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, body) VALUES (2, 'hello') USING OPERATION_ID 'op-omit-embedding'",
        )
        .expect_err("omitting the non-null VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "23502");
    // 次元 4 の VECTOR 列として検索できる。
    let hits = core
        .execute_sql(
            &alice,
            "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        )
        .expect("select should succeed")
        .rows
        .len();
    assert_eq!(hits, 1);
}

#[test]
fn create_table_accepts_lowercase_keywords_and_trailing_semicolon() {
    let (core, path) = new_core("create-lowercase");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(&alice, &mut session, "create table t (body text);")
        .expect("lowercase CREATE TABLE should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

#[test]
fn create_table_persists_across_reopen() {
    let (core, path) = new_core("create-persist");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(3))",
    )
    .expect("CREATE TABLE should succeed");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage
        .get_table_schema("docs")
        .expect("schema must survive reopen");
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].ty, ColumnType::Vector(3));
}

/// 別セッションが新規テーブルへ即座に書き込み・検索できること（各文が新しい
/// read txn でスキーマを取得する既存設計により、DDL 実行と同一 `EngineCore`
/// 内の別セッションからも新テーブルが即座に見える契約を固定する）。
#[test]
fn create_table_is_immediately_usable_by_other_session() {
    let (core, path) = new_core("create-usable");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl_session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE TABLE docs (embedding VECTOR(2), body TEXT)",
    )
    .expect("CREATE TABLE should succeed");

    let mut other_session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut other_session,
        "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.1,0.2]', 'hello') USING OPERATION_ID 'op-1'",
    )
    .expect("INSERT into freshly created table should succeed");
    let result = core
        .execute_sql(
            &alice,
            "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10",
        )
        .expect("SELECT should succeed");
    assert_eq!(result.rows.len(), 1);
}

// --- 重複拒否（TABLE-4） ------------------------------------------------

#[test]
fn create_table_rejects_duplicate_name_without_overwriting() {
    let (core, path) = new_core("create-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(2), a TEXT)",
    )
    .expect("first CREATE TABLE should succeed");

    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (b TEXT, c TEXT)")
        .expect_err("duplicate CREATE TABLE must be rejected");
    assert_eq!(err.wire_code(), "42P07");

    // 既存スキーマは変更されない（TABLE-4）: 元の列（`embedding`／`a`）だけで
    // INSERT できる（拒否された 2 回目の列 `b`／`c` は一切反映されない）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, a) VALUES (1, '[0.1,0.2]', 'x') USING OPERATION_ID 'op-original-schema'",
    )
    .expect("original schema must remain intact after rejected duplicate CREATE TABLE");
}

// --- 権限ゲート（SQL-23・TASK-202。fail-closed） -----------------------

#[test]
fn create_table_rejects_unauthorized_session_for_valid_syntax() {
    let (core, path) = new_core("create-unauthorized-valid");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect_err("unauthorized session must be denied");
    assert_eq!(err.wire_code(), "42501");

    // テーブルは実際には作られていない: 許可済みセッションが同じ SQL で
    // 改めて `CREATE TABLE` すると `42P07`（既存名との衝突）ではなく成功する。
    let mut granted = granted_session();
    let outcome = core
        .execute_sql_in_session(&alice, &mut granted, "CREATE TABLE docs (a TEXT)")
        .expect("table must not have been created by the unauthorized attempt");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

/// 未許可の主体は、対象テーブルの有無に関わらず同じ `42501` のみを受け取り、
/// カタログ状態を観測できない（fail-closed。`sql::ddl` モジュールドキュメント
/// 参照）。一方、構造検証（カタログ照会なし）は `DROP TABLE`（Issue #902）と
/// 同じく権限ゲートより**前**に通す設計判断のため、そもそも許可形状に一致しない
/// 構文（`CREATE`／`TABLE` の 2 トークンにも満たない・カタログを一切参照しない
/// 壊れ方）は権限の有無に関わらず `42601` になる（構文の正誤自体はカタログの
/// 存在情報ではないためオラクルにならない。`parse_tokens`→
/// `execute_parsed_in_session` の判定順序は `docs/design/sql-create-table.md`
/// 参照）。
#[test]
fn create_table_rejects_unauthorized_session_for_garbage_syntax() {
    let (core, path) = new_core("create-unauthorized-garbage");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs ((()) not a schema")
        .expect_err("unauthorized session must be denied even for malformed syntax");
    assert_eq!(
        err.wire_code(),
        "42601",
        "malformed syntax is rejected by structural validation before the DDL permission gate \
         (no catalog access occurs at either stage, so this is not an existence oracle)"
    );
}

#[test]
fn create_table_rejects_unauthorized_session_for_existing_table_name() {
    let (core, path) = new_core("create-unauthorized-existing");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut granted = granted_session();
    core.execute_sql_in_session(&alice, &mut granted, "CREATE TABLE docs (a TEXT)")
        .expect("first CREATE TABLE should succeed");

    let mut unauthorized = SessionState::default();
    let err = core
        .execute_sql_in_session(&alice, &mut unauthorized, "CREATE TABLE docs (b TEXT)")
        .expect_err("unauthorized session must be denied");
    // 既存名と衝突する場合でも `42P07` ではなく `42501`（既存名の有無を漏らさない）。
    assert_eq!(err.wire_code(), "42501");
}

// --- 上限・拒否系（アロケーション前に拒否・カタログ不変） ---------------

#[test]
fn create_table_rejects_empty_column_list() {
    let (core, path) = new_core("create-empty-columns");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs ()")
        .expect_err("empty column list must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_trailing_comma() {
    let (core, path) = new_core("create-trailing-comma");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT,)")
        .expect_err("trailing comma must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_unknown_type() {
    let (core, path) = new_core("create-unknown-type");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        // スカラー型・配列・ENUM は受理する（Issue #1348）ため、未知の型名の代表として
        // 未登録の識別子を使う（ENUM 型名候補として実行段で存在確認され `42601`）。
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a no_such_type)")
        .expect_err("unknown type must be rejected");
    assert_eq!(err.wire_code(), "42601");
    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a no_such_type[])")
        .expect_err("unknown array element type must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_two_vector_columns() {
    let (core, path) = new_core("create-two-vector-e2e");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a VECTOR(2), b VECTOR(2))",
        )
        .expect_err("two VECTOR columns must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_vector_dimension_zero_and_overflow() {
    let (core, path) = new_core("create-vector-dim");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    for sql in [
        "CREATE TABLE t1 (a VECTOR(0))",
        "CREATE TABLE t2 (a VECTOR(65537))",
        "CREATE TABLE t3 (a VECTOR(99999999999))",
        "CREATE TABLE t4 (a VECTOR(-1))",
        "CREATE TABLE t5 (a VECTOR(1.5))",
    ] {
        let err = core
            .execute_sql_in_session(&alice, &mut session, sql)
            .unwrap_err_or_else(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
}

trait UnwrapErrOrElse<T, E> {
    fn unwrap_err_or_else(self, sql: &str) -> E;
}
impl<T: std::fmt::Debug, E> UnwrapErrOrElse<T, E> for Result<T, E> {
    fn unwrap_err_or_else(self, sql: &str) -> E {
        self.expect_err(&format!("expected error for sql={sql}"))
    }
}

#[test]
fn create_table_accepts_vector_dimension_upper_bound() {
    let (core, path) = new_core("create-vector-dim-max");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let outcome = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE t (a VECTOR(65536))")
        .expect("VECTOR(65536) must be accepted (upper bound)");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

#[test]
fn create_table_rejects_too_many_columns() {
    let (core, path) = new_core("create-too-many-columns");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let cols: Vec<String> = (0..257).map(|i| format!("c{i} TEXT")).collect();
    let sql = format!("CREATE TABLE docs ({})", cols.join(", "));
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("257 columns must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn create_table_rejects_duplicate_column_name_in_same_statement() {
    let (core, path) = new_core("create-duplicate-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        .execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT, a TEXT)")
        .expect_err("duplicate column name must be rejected");
    assert_eq!(err.wire_code(), "42701");
}

#[test]
fn create_table_rejects_reserved_column_names() {
    let (core, path) = new_core("create-reserved-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    for sql in [
        "CREATE TABLE t1 (id TEXT)",
        "CREATE TABLE t2 (ID TEXT)",
        "CREATE TABLE t3 (tenant_id TEXT)",
        "CREATE TABLE t4 (visibility TEXT)",
    ] {
        let err = core
            .execute_sql_in_session(&alice, &mut session, sql)
            .unwrap_err_or_else(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
}

#[test]
fn create_table_rejects_oversized_identifier() {
    let (core, path) = new_core("create-oversized-ident");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let long_name = "a".repeat(64);
    let sql = format!("CREATE TABLE {long_name} (a TEXT)");
    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("64-byte identifier must be rejected");
    assert_eq!(err.wire_code(), "42601");

    let ok_name = "a".repeat(63);
    let sql = format!("CREATE TABLE {ok_name} (a TEXT)");
    core.execute_sql_in_session(&alice, &mut session, &sql)
        .expect("63-byte identifier must be accepted");
}

// --- 回帰: 既存構文の非破壊 ---------------------------------------------

#[test]
fn create_function_still_works() {
    let (core, path) = new_core("create-function-regression");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&alice, &mut session, "CREATE FUNCTION double(x) AS x + x")
        .expect("CREATE FUNCTION must still work without DDL privilege");
    assert!(matches!(outcome, SqlOutcome::CreateFunction { .. }));
}

#[test]
fn explain_create_table_is_rejected() {
    let (core, path) = new_core("create-explain-rejected");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    let err = core
        .execute_sql_in_session(&alice, &mut session, "EXPLAIN CREATE TABLE docs (a TEXT)")
        .expect_err("EXPLAIN CREATE TABLE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_via_sessionless_entry_point_is_rejected() {
    let (core, path) = new_core("create-sessionless");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let err = core
        .execute_sql(&alice, "CREATE TABLE docs (a TEXT)")
        .expect_err("session-less entry point must reject CREATE TABLE");
    assert_eq!(err.wire_code(), "42601");
}

// --- テナント境界（RLS 相当。CREATE TABLE 自体はテナント非依存のカタログ操作） --

/// TABLE-2: SQL で作成した 2 テーブルの次元が独立し、互いのベクトル空間・
/// 検索結果が混線しないことを固定する。
#[test]
fn create_table_dimensions_are_independent_across_tables() {
    let (core, path) = new_core("create-independent-dims");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE small (embedding VECTOR(2))",
    )
    .expect("small table create");
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE big (embedding VECTOR(4))",
    )
    .expect("big table create");

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "INSERT INTO small (id, embedding) VALUES (1, '[1,0]') USING OPERATION_ID 'op-small'",
    )
    .expect("insert into small");
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "INSERT INTO big (id, embedding) VALUES (1, '[1,0,0,0]') USING OPERATION_ID 'op-big'",
    )
    .expect("insert into big");

    let small_hits = core
        .execute_sql(
            &alice,
            "SELECT id FROM small ORDER BY embedding <=> '[1,0]' LIMIT 10",
        )
        .expect("select small")
        .rows
        .len();
    let big_hits = core
        .execute_sql(
            &alice,
            "SELECT id FROM big ORDER BY embedding <=> '[1,0,0,0]' LIMIT 10",
        )
        .expect("select big")
        .rows
        .len();
    assert_eq!(small_hits, 1);
    assert_eq!(big_hits, 1);
}
